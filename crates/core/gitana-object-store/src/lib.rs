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

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};

use gitana_object::{
	HashAlgorithm, MAX_OBJECT_SIZE, ObjectError, ObjectId, ObjectKind, PackIndex, decode_loose,
	decode_object_at, decode_pack_index, encode_loose, encode_pack_index, loose_object_path,
	pack_index_entries,
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
		let mut hex = String::with_capacity(H::RAW_LEN * 2);
		for byte in checksum {
			hex.push_str(&format!("{byte:02x}"));
		}
		let idx_bytes = encode_pack_index::<H>(&entries, checksum)?;
		let pack_path = format!("{PACK_PREFIX}pack-{hex}{PACK_SUFFIX}");
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
}
