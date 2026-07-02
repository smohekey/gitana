//! Git object storage: read/write git objects by id over a `GitFileStore`.
//!
//! Composition crate: it wires [`gitana_object`]'s codecs onto a
//! [`gitana_file_store::GitFileStore`] backend, scoped to one repository. Generic
//! over the backend `F` and the hash algorithm `H` — layers are wired with
//! compile-time generics (see docs/hlds/storage-layer.md). Reads try the loose object
//! first, then stored packfiles: an object is located through the pack's `.idx` (id →
//! offset) and decoded on demand from the compressed pack bytes, so no pack is decoded
//! in full to find (or miss) one object. Every loose read recomputes the id under `H`
//! and rejects a mismatch; objects served from a pack are content-addressed by
//! construction (`decode_object_at` computes each id from its bytes).

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::Arc;

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};

use gitana_object::{
	HashAlgorithm, MAX_OBJECT_SIZE, ObjectError, ObjectId, ObjectKind, PackIndex, PackedObject,
	decode_loose, decode_object_at, decode_pack_index, encode_loose, encode_pack, encode_pack_index,
	loose_object_path, pack_index_entries,
};
use tokio::sync::Mutex;

/// Re-exported so downstream layers name object kinds through the store layer.
pub use gitana_object::ObjectKind as Kind;

const PACK_PREFIX: &str = "objects/pack/";
const PACK_SUFFIX: &str = ".pack";
const IDX_SUFFIX: &str = ".idx";

/// Maximum stored packfile size (2 GiB).
pub const MAX_PACK_SIZE: u64 = 2 << 30;

/// Errors from object storage.
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
	/// No object exists for the requested id.
	#[error("object not found")]
	NotFound,
	/// A stored object's recomputed id did not match the id it was read under.
	#[error("object corruption: stored under {requested}, content hashes to {actual}")]
	Corruption {
		/// The hex id the object was requested/stored under.
		requested: String,
		/// The hex id its bytes actually hash to.
		actual: String,
	},
	/// The underlying file store failed.
	#[error("file store error: {0}")]
	FileStore(#[from] FileStoreError),
	/// The object bytes could not be decoded.
	#[error("object decode error: {0}")]
	Object(#[from] ObjectError),
	/// A write exceeded the object (1 GiB) or pack (2 GiB) size limit.
	#[error("input exceeds the maximum size of {limit} bytes")]
	TooLarge {
		/// The limit that was exceeded.
		limit: u64,
	},
}

fn ensure_within(len: u64, limit: u64) -> Result<(), ObjectStoreError> {
	if len > limit {
		Err(ObjectStoreError::TooLarge { limit })
	} else {
		Ok(())
	}
}

/// The `.idx` sidecar path for a `.pack` path (`…/pack-<hex>.pack` → `…/pack-<hex>.idx`).
fn index_path(pack_path: &str) -> String {
	let stem = pack_path.strip_suffix(PACK_SUFFIX).unwrap_or(pack_path);
	format!("{stem}{IDX_SUFFIX}")
}

/// The `objects/pack/pack-<hex>.pack` path for a pack whose trailer checksum is `checksum`.
fn pack_path_for(checksum: &[u8]) -> String {
	let mut hex = String::with_capacity(checksum.len() * 2);
	for byte in checksum {
		hex.push_str(&format!("{byte:02x}"));
	}
	format!("{PACK_PREFIX}pack-{hex}{PACK_SUFFIX}")
}

/// What a [`ObjectStore::repack`] consolidated: how many objects went into the new pack, and
/// how many now-redundant packs and loose objects it removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepackReport {
	/// Objects written into the single new pack.
	pub packed_objects: usize,
	/// Old packs deleted (each with its `.idx`).
	pub packs_removed: usize,
	/// Loose objects deleted.
	pub loose_removed: usize,
}

/// What a [`ObjectStore::prune_loose`] removed: the number of unreachable loose objects deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
	/// Loose objects deleted (those absent from the caller's keep set).
	pub pruned: usize,
}

/// Git object storage layered over a file-store backend `F`, scoped to one repo, with
/// object ids under the hash algorithm `H`.
pub struct ObjectStore<F, H: HashAlgorithm> {
	files: F,
	/// Parsed `.idx` per pack path (id → offset), for locating an object without decoding
	/// the pack. Small; populated from the `.idx` sidecar on first touch of a pack.
	indexes: Mutex<HashMap<String, Arc<PackIndex<H>>>>,
	/// Compressed pack bytes, read whole once and shared. Loaded only when a pack actually
	/// holds a requested object; a miss consults `indexes` alone and never reads the `.pack`.
	pack_bytes: Mutex<HashMap<String, Arc<Vec<u8>>>>,
	_hash: PhantomData<H>,
}

impl<F, H> ObjectStore<F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	/// Build an object store over `files` for repository `repo`.
	pub fn new(files: F) -> Self {
		Self {
			files,
			indexes: Mutex::new(HashMap::new()),
			pack_bytes: Mutex::new(HashMap::new()),
			_hash: PhantomData,
		}
	}

	/// Borrow the underlying file store.
	pub fn file_store(&self) -> &F {
		&self.files
	}

	/// Write an object loose, returning its id. Idempotent: re-writing identical
	/// content is a no-op because the content-addressed path already exists.
	pub async fn write_object(
		&self,
		kind: ObjectKind,
		payload: &[u8],
	) -> Result<ObjectId<H>, ObjectStoreError> {
		ensure_within(payload.len() as u64, MAX_OBJECT_SIZE)?;
		let id = ObjectId::<H>::compute(kind, payload);
		let bytes = encode_loose(kind, payload);
		let path = loose_object_path(&id);
		match self.files.write_path_if_absent(&path, &bytes).await? {
			WriteOutcome::Written | WriteOutcome::AlreadyExists => Ok(id),
		}
	}

	/// Store a packfile under `objects/pack/`, keyed by its trailer checksum, alongside its
	/// `.idx` sidecar. Computing the index also validates the pack (a malformed or thin pack
	/// is rejected and never stored). Idempotent, and it writes a missing `.idx` even when the
	/// `.pack` is already present.
	pub async fn write_pack(&self, pack: &[u8]) -> Result<(), ObjectStoreError> {
		ensure_within(pack.len() as u64, MAX_PACK_SIZE)?;
		// One decode both validates the pack and yields the `.idx` entries.
		let entries = pack_index_entries::<H>(pack)?;
		let checksum = &pack[pack.len() - H::RAW_LEN..];
		let idx_bytes = encode_pack_index::<H>(&entries, checksum)?;
		let pack_path = pack_path_for(checksum);
		self.files.write_path_if_absent(&pack_path, pack).await?;
		self
			.files
			.write_path_if_absent(&index_path(&pack_path), &idx_bytes)
			.await?;
		Ok(())
	}

	/// Read an object by id, from a loose object or a stored pack.
	pub async fn read_object(
		&self,
		id: &ObjectId<H>,
	) -> Result<(ObjectKind, Vec<u8>), ObjectStoreError> {
		match self.files.read_path(&loose_object_path(id)).await {
			Ok(bytes) => {
				let (kind, payload) = decode_loose(&bytes)?;
				let actual = ObjectId::<H>::compute(kind, &payload);
				if &actual != id {
					return Err(ObjectStoreError::Corruption {
						requested: id.to_hex(),
						actual: actual.to_hex(),
					});
				}
				Ok((kind, payload))
			}
			Err(FileStoreError::NotFound) => match self.locate(id).await? {
				Some((pack_path, index, offset)) => {
					let bytes = self.pack_bytes(&pack_path).await?;
					let object = decode_object_at::<H>(&bytes, &index, offset)?;
					// Content-address the result: a stale or corrupt `.idx` could point this id at
					// the wrong offset, so the object we materialised must actually hash to `id`
					// (the whole-pack decode this replaced was keyed by the recomputed id).
					if &object.id != id {
						return Err(ObjectStoreError::Corruption {
							requested: id.to_hex(),
							actual: object.id.to_hex(),
						});
					}
					Ok((object.kind, object.data))
				}
				None => Err(ObjectStoreError::NotFound),
			},
			Err(other) => Err(other.into()),
		}
	}

	/// Whether an object with `id` is stored, loose or packed.
	pub async fn exists_object(&self, id: &ObjectId<H>) -> Result<bool, ObjectStoreError> {
		if self.files.exists(&loose_object_path(id)).await? {
			return Ok(true);
		}
		Ok(self.locate(id).await?.is_some())
	}

	/// Find which stored pack holds `id`, returning the pack path, its cached index, and the
	/// object's byte offset. Consults only each pack's (small) `.idx`, so a miss decodes no
	/// object bytes and reads no `.pack`.
	async fn locate(
		&self,
		id: &ObjectId<H>,
	) -> Result<Option<(String, Arc<PackIndex<H>>, u64)>, ObjectStoreError> {
		let pack_paths = self.files.list_prefix(PACK_PREFIX).await?;
		for path in pack_paths {
			if !path.ends_with(PACK_SUFFIX) {
				continue;
			}
			let index = self.pack_index(&path).await?;
			if let Some(offset) = index.offset_of(id) {
				return Ok(Some((path, index, offset)));
			}
		}
		Ok(None)
	}

	/// The parsed `.idx` for one pack, from its sidecar, decoding and caching on first use.
	/// If the sidecar is absent (a legacy or foreign pack lacking one), the index is rebuilt
	/// by decoding the pack once — read-only, so no sidecar is written on the read path.
	async fn pack_index(&self, pack_path: &str) -> Result<Arc<PackIndex<H>>, ObjectStoreError> {
		if let Some(index) = self.indexes.lock().await.get(pack_path) {
			return Ok(Arc::clone(index));
		}
		let index = match self.files.read_path(&index_path(pack_path)).await {
			Ok(bytes) => decode_pack_index::<H>(&bytes)?,
			Err(FileStoreError::NotFound) => {
				let pack = self.files.read_path(pack_path).await?;
				let entries = pack_index_entries::<H>(&pack)?;
				let checksum = pack[pack.len() - H::RAW_LEN..].to_vec();
				PackIndex::from_entries(entries, checksum)?
			}
			Err(other) => return Err(other.into()),
		};
		let index = Arc::new(index);
		self
			.indexes
			.lock()
			.await
			.insert(pack_path.to_owned(), Arc::clone(&index));
		Ok(index)
	}

	/// The compressed bytes of one pack, read whole once and cached for on-demand object decode.
	async fn pack_bytes(&self, pack_path: &str) -> Result<Arc<Vec<u8>>, ObjectStoreError> {
		if let Some(bytes) = self.pack_bytes.lock().await.get(pack_path) {
			return Ok(Arc::clone(bytes));
		}
		let bytes = Arc::new(self.files.read_path(pack_path).await?);
		self
			.pack_bytes
			.lock()
			.await
			.insert(pack_path.to_owned(), Arc::clone(&bytes));
		Ok(bytes)
	}

	/// Consolidate storage: gather every loose object and every object in every existing pack
	/// into one new pack, then delete the now-redundant loose objects and old packs. No object
	/// is dropped — this changes only how objects are stored, not which exist (pruning
	/// unreachable objects is a separate concern). Returns `None` when nothing needs doing
	/// (already a single pack with no loose objects).
	///
	/// The new pack is written before anything is deleted, so an object is never momentarily
	/// unreferenced on disk; a crash mid-way leaves redundant-but-correct storage that a re-run
	/// cleans up. Objects are materialised in memory together (inherent to [`encode_pack`]'s
	/// slice input) — streaming a repack is a future refinement.
	pub async fn repack(&self) -> Result<Option<RepackReport>, ObjectStoreError> {
		// Snapshot the packs to consolidate before writing the new one.
		let old_packs: Vec<String> = self
			.files
			.list_prefix(PACK_PREFIX)
			.await?
			.into_iter()
			.filter(|path| path.ends_with(PACK_SUFFIX))
			.collect();
		let loose_ids = self.loose_object_ids().await?;

		// Already consolidated: nothing loose and at most one pack — but only a no-op if that
		// lone pack still has its `.idx`. A pack without its sidecar is readable by our own
		// fallback yet not by stock git, so fall through and let the repack regenerate it.
		let single_pack_indexed = match old_packs.as_slice() {
			[] => true,
			[only] => self.files.exists(&index_path(only)).await?,
			_ => false,
		};
		if loose_ids.is_empty() && single_pack_indexed {
			return Ok(None);
		}

		// Union of every stored id: each pack's index entries plus the loose objects.
		let mut ids: std::collections::HashSet<ObjectId<H>> = std::collections::HashSet::new();
		for path in &old_packs {
			for entry in self.pack_index(path).await?.entries() {
				ids.insert(entry.id);
			}
		}
		ids.extend(loose_ids.iter().copied());
		if ids.is_empty() {
			return Ok(None);
		}

		// Materialise every object, then encode a single pack and store it (with its `.idx`).
		let mut objects: Vec<PackedObject<H>> = Vec::with_capacity(ids.len());
		for id in &ids {
			let (kind, data) = self.read_object(id).await?;
			objects.push(PackedObject {
				id: *id,
				kind,
				data,
			});
		}
		let pack = encode_pack(&objects);
		let new_pack_path = pack_path_for(&pack[pack.len() - H::RAW_LEN..]);
		self.write_pack(&pack).await?;

		// Delete the old packs (except the one we just wrote, if a re-encode reproduced it) and
		// every loose object — all now redundant with the new pack.
		let mut packs_removed = 0;
		for path in &old_packs {
			if *path == new_pack_path {
				continue;
			}
			self.files.delete_path(path, None).await?;
			self.files.delete_path(&index_path(path), None).await?;
			packs_removed += 1;
		}
		for id in &loose_ids {
			self.files.delete_path(&loose_object_path(id), None).await?;
		}

		// The caches may name deleted packs; drop them (reads re-list and reload lazily).
		self.indexes.lock().await.clear();
		self.pack_bytes.lock().await.clear();

		Ok(Some(RepackReport {
			packed_objects: objects.len(),
			packs_removed,
			loose_removed: loose_ids.len(),
		}))
	}

	/// Every loose object id on disk, by scanning the `objects/<aa>/<rest>` fan-out (skipping
	/// the `pack`/`info` directories and any non-hex names).
	async fn loose_object_ids(&self) -> Result<Vec<ObjectId<H>>, ObjectStoreError> {
		let hex_len = H::RAW_LEN * 2;
		let mut ids = Vec::new();
		for dir in self.files.list_prefix("objects/").await? {
			let name = dir.rsplit('/').next().unwrap_or_default();
			if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
				continue;
			}
			for entry in self.files.list_prefix(&format!("{dir}/")).await? {
				let rest = entry.rsplit('/').next().unwrap_or_default();
				if rest.len() != hex_len - 2 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
					continue;
				}
				if let Ok(id) = ObjectId::<H>::from_hex(&format!("{name}{rest}")) {
					ids.push(id);
				}
			}
		}
		Ok(ids)
	}

	/// Delete every loose object whose id is **not** in `keep`. `keep` is the caller's set of
	/// reachable object ids (see the prune safety rules); anything loose and absent from it is
	/// unreferenced and removed. Packed objects are never touched — only loose deletion. The
	/// caller is responsible for computing a complete `keep` set; there is no time-based grace,
	/// so a loose object written concurrently after `keep` was computed could be removed (prune
	/// is an explicit, quiescent-repo operation).
	pub async fn prune_loose(
		&self,
		keep: &HashSet<ObjectId<H>>,
	) -> Result<PruneReport, ObjectStoreError> {
		let mut pruned = 0;
		for id in self.loose_object_ids().await? {
			if !keep.contains(&id) {
				self
					.files
					.delete_path(&loose_object_path(&id), None)
					.await?;
				pruned += 1;
			}
		}
		Ok(PruneReport { pruned })
	}
}
