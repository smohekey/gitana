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
	HashAlgorithm, MAX_OBJECT_SIZE, ObjectError, ObjectId, ObjectKind, PackEntry, PackIndex,
	PackedObject, apply_delta, decode_loose, decode_object_at, decode_pack_entry, decode_pack_index,
	encode_loose, encode_pack, encode_pack_index, loose_object_path, pack_index_entries,
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

/// The order `encode_pack` writes object types in (commits, tags, trees, blobs). Used to sort
/// repack metadata so delta-friendly objects (same type, adjacent size) land in the same pack.
fn kind_rank(kind: ObjectKind) -> u8 {
	match kind {
		ObjectKind::Commit => 0,
		ObjectKind::Tag => 1,
		ObjectKind::Tree => 2,
		ObjectKind::Blob => 3,
	}
}

/// Cut sorted repack metadata into contiguous `[start, end)` ranges, each whose objects'
/// estimated packed size (uncompressed size + a small per-object header, a conservative upper
/// bound since compression only shrinks) stays under `max_pack_size` (less pack header/trailer
/// headroom). Every range holds at least one object, so a lone object larger than the budget gets
/// its own range (an object cannot span packs).
fn partition_ranges<H: HashAlgorithm>(
	meta: &[(ObjectId<H>, ObjectKind, u64)],
	max_pack_size: u64,
) -> Vec<(usize, usize)> {
	const PER_OBJECT_OVERHEAD: u64 = 32;
	const PACK_OVERHEAD: u64 = 128;
	let budget = max_pack_size.saturating_sub(PACK_OVERHEAD).max(1);

	let mut ranges = Vec::new();
	let mut start = 0;
	let mut acc = 0u64;
	for (i, entry) in meta.iter().enumerate() {
		let est = entry.2.saturating_add(PER_OBJECT_OVERHEAD);
		if i > start && acc.saturating_add(est) > budget {
			ranges.push((start, i));
			start = i;
			acc = 0;
		}
		acc = acc.saturating_add(est);
	}
	if start < meta.len() {
		ranges.push((start, meta.len()));
	}
	ranges
}

/// The `objects/pack/pack-<hex>.pack` path for a pack whose trailer checksum is `checksum`.
fn pack_path_for(checksum: &[u8]) -> String {
	let mut hex = String::with_capacity(checksum.len() * 2);
	for byte in checksum {
		hex.push_str(&format!("{byte:02x}"));
	}
	format!("{PACK_PREFIX}pack-{hex}{PACK_SUFFIX}")
}

/// What a [`ObjectStore::repack`] consolidated: how many objects were packed, into how many new
/// packs, and how many now-redundant packs and loose objects it removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepackReport {
	/// Objects written into the new pack(s).
	pub packed_objects: usize,
	/// New packs written (>1 when the object set exceeds the pack-size limit).
	pub packs_written: usize,
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

/// A pack's cached metadata: its parsed `.idx` (id → offset), the offsets sorted ascending (to
/// bound a single entry as `[offset, next_offset)` for a lazy range read), and its total byte
/// size. All `O(objects)` and small — never the pack's bytes.
struct PackMeta<H: HashAlgorithm> {
	index: PackIndex<H>,
	offsets_sorted: Vec<u64>,
	size: u64,
}

impl<H: HashAlgorithm> PackMeta<H> {
	/// The smallest entry offset strictly greater than `offset`, i.e. where the entry at `offset`
	/// ends. `None` for the last entry (its data runs to the pack body's end).
	fn next_offset_after(&self, offset: u64) -> Option<u64> {
		let i = self.offsets_sorted.partition_point(|&o| o <= offset);
		self.offsets_sorted.get(i).copied()
	}
}

/// Git object storage layered over a file-store backend `F`, scoped to one repo, with
/// object ids under the hash algorithm `H`.
pub struct ObjectStore<F, H: HashAlgorithm> {
	files: F,
	/// Per-pack metadata (parsed `.idx`, sorted offsets, size) for locating an object without
	/// decoding the pack. Small; populated from the `.idx` sidecar on first touch of a pack.
	packs: Mutex<HashMap<String, Arc<PackMeta<H>>>>,
	/// Compressed pack bytes, read whole once and shared — the interactive read cache. Repack does
	/// not use it (it reads objects lazily via `read_path_range`), so consolidating a repository
	/// never holds a whole pack here.
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
			packs: Mutex::new(HashMap::new()),
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
				Some((pack_path, meta, offset)) => {
					let bytes = self.pack_bytes(&pack_path).await?;
					let object = decode_object_at::<H>(&bytes, &meta.index, offset)?;
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

	/// Read an object by id **without caching whole packs** — the loose file, or a packed object
	/// materialised through per-entry range reads (`[offset, next_offset)`, resolving deltas by
	/// further range reads). Peak memory is one delta chain, not a pack, so repack stays bounded
	/// even when consolidating many large packs.
	async fn read_object_bounded(
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
				Some((pack_path, meta, offset)) => {
					let (kind, data) = self.read_packed_lazy(&pack_path, &meta, offset).await?;
					let actual = ObjectId::<H>::compute(kind, &data);
					if &actual != id {
						return Err(ObjectStoreError::Corruption {
							requested: id.to_hex(),
							actual: actual.to_hex(),
						});
					}
					Ok((kind, data))
				}
				None => Err(ObjectStoreError::NotFound),
			},
			Err(other) => Err(other.into()),
		}
	}

	/// Materialise the object at byte `offset` in `pack_path` by reading only its entry (and its
	/// delta chain's entries) via `read_path_range`, never loading the whole pack. Mirrors
	/// [`gitana_object::decode_object_at`] but with range reads; the visited-offset set guards a
	/// REF-delta cycle.
	async fn read_packed_lazy(
		&self,
		pack_path: &str,
		meta: &PackMeta<H>,
		offset: u64,
	) -> Result<(ObjectKind, Vec<u8>), ObjectStoreError> {
		let body_end = meta
			.size
			.checked_sub(H::RAW_LEN as u64)
			.ok_or(ObjectError::MalformedPack)?;

		let mut deltas: Vec<Vec<u8>> = Vec::new();
		let mut visited: HashSet<u64> = HashSet::new();
		let mut cursor = offset;
		let (kind, mut data) = loop {
			if !visited.insert(cursor) {
				return Err(ObjectError::MalformedPack.into());
			}
			let end = meta.next_offset_after(cursor).unwrap_or(body_end);
			if cursor >= end {
				return Err(ObjectError::MalformedPack.into());
			}
			let entry = self
				.files
				.read_path_range(pack_path, cursor, end - cursor)
				.await?;
			match decode_pack_entry::<H>(&entry)? {
				PackEntry::Base { kind, data } => break (kind, data),
				PackEntry::OfsDelta { distance, delta } => {
					deltas.push(delta);
					cursor = cursor
						.checked_sub(distance)
						.ok_or(ObjectError::MalformedPack)?;
				}
				PackEntry::RefDelta { base, delta } => {
					deltas.push(delta);
					cursor = meta
						.index
						.offset_of(&base)
						.ok_or(ObjectError::UnresolvedDeltaBase)?;
				}
			}
		};
		for delta in deltas.iter().rev() {
			data = apply_delta(&data, delta)?;
		}
		Ok((kind, data))
	}

	/// Whether an object with `id` is stored, loose or packed.
	pub async fn exists_object(&self, id: &ObjectId<H>) -> Result<bool, ObjectStoreError> {
		if self.files.exists(&loose_object_path(id)).await? {
			return Ok(true);
		}
		Ok(self.locate(id).await?.is_some())
	}

	/// Find which stored pack holds `id`, returning the pack path, its cached metadata, and the
	/// object's byte offset. Consults only each pack's (small) `.idx`, so a miss decodes no
	/// object bytes and reads no `.pack`.
	async fn locate(
		&self,
		id: &ObjectId<H>,
	) -> Result<Option<(String, Arc<PackMeta<H>>, u64)>, ObjectStoreError> {
		let pack_paths = self.files.list_prefix(PACK_PREFIX).await?;
		for path in pack_paths {
			if !path.ends_with(PACK_SUFFIX) {
				continue;
			}
			let meta = self.pack_meta(&path).await?;
			if let Some(offset) = meta.index.offset_of(id) {
				return Ok(Some((path, meta, offset)));
			}
		}
		Ok(None)
	}

	/// The cached metadata for one pack, built and cached on first use: its `.idx` (from the
	/// sidecar, or rebuilt by decoding the pack once if the sidecar is absent — read-only, so no
	/// sidecar is written on the read path), the offsets sorted ascending, and the pack size.
	async fn pack_meta(&self, pack_path: &str) -> Result<Arc<PackMeta<H>>, ObjectStoreError> {
		if let Some(meta) = self.packs.lock().await.get(pack_path) {
			return Ok(Arc::clone(meta));
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
		let mut offsets_sorted: Vec<u64> = index.entries().iter().map(|e| e.offset).collect();
		offsets_sorted.sort_unstable();
		let size = self.files.size(pack_path).await?;
		let meta = Arc::new(PackMeta {
			index,
			offsets_sorted,
			size,
		});
		self
			.packs
			.lock()
			.await
			.insert(pack_path.to_owned(), Arc::clone(&meta));
		Ok(meta)
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
	/// into one or more new packs — each at most `max_pack_size` bytes — then delete the now
	/// redundant loose objects and old packs. No object is dropped; this changes only how objects
	/// are stored, not which exist (pruning unreachable objects is a separate concern). Returns
	/// `None` when nothing needs doing (already a single indexed pack with no loose objects).
	///
	/// **Memory-bounded:** the repo's content is never held all at once. Pass 1 reads each object
	/// once to record only its `(kind, size)` (dropping the data) and sorts that small metadata
	/// to group delta-friendly objects; the sorted list is cut into size-bounded partitions.
	/// Pass 2 then materialises, encodes, writes, and drops **one partition at a time**, so peak
	/// memory is ≈ one pack (≤ `max_pack_size`) plus `O(objects)` of metadata — independent of
	/// repo size. New packs are written before anything is deleted, so an object is never
	/// momentarily unreferenced on disk; a crash mid-way leaves redundant-but-correct storage a
	/// re-run cleans up. `max_pack_size` is clamped to [`MAX_PACK_SIZE`] (the absolute per-pack
	/// ceiling `write_pack` enforces).
	pub async fn repack(&self, max_pack_size: u64) -> Result<Option<RepackReport>, ObjectStoreError> {
		let max_pack_size = max_pack_size.min(MAX_PACK_SIZE);

		// Snapshot the packs to consolidate before writing the new ones.
		let old_packs: Vec<String> = self
			.files
			.list_prefix(PACK_PREFIX)
			.await?
			.into_iter()
			.filter(|path| path.ends_with(PACK_SUFFIX))
			.collect();
		let loose_ids = self.loose_object_ids().await?;

		// No-op only when the layout is already what repack would cheaply produce: nothing loose
		// and at most one pack that is indexed and within the limit. Multiple packs always re-pack
		// so repack can *consolidate* them into fewer (its original purpose); that is idempotent
		// when they are already optimally split. A lone pack lacking its `.idx` (unreadable by
		// stock git) or over `max_pack_size` (must be split) also re-packs.
		let noop = loose_ids.is_empty()
			&& match old_packs.as_slice() {
				[] => true,
				[only] => {
					self.files.exists(&index_path(only)).await?
						&& self.files.size(only).await? <= max_pack_size
				}
				_ => false,
			};
		if noop {
			return Ok(None);
		}

		// Union of every stored id: each pack's index entries plus the loose objects.
		let mut ids: HashSet<ObjectId<H>> = HashSet::new();
		for path in &old_packs {
			for entry in self.pack_meta(path).await?.index.entries() {
				ids.insert(entry.id);
			}
		}
		ids.extend(loose_ids.iter().copied());
		if ids.is_empty() {
			return Ok(None);
		}

		// Pass 1: read each object once for its (kind, size), dropping the data. Sort the small
		// metadata the way `encode_pack` orders objects (type, then largest-first, then id), so a
		// contiguous run groups delta-friendly objects into the same size-bounded partition. Reads
		// are memory-bounded (no whole-pack caching).
		let mut meta: Vec<(ObjectId<H>, ObjectKind, u64)> = Vec::with_capacity(ids.len());
		for id in &ids {
			let (kind, data) = self.read_object_bounded(id).await?;
			meta.push((*id, kind, data.len() as u64));
		}
		meta.sort_by(|a, b| {
			kind_rank(a.1)
				.cmp(&kind_rank(b.1))
				.then(b.2.cmp(&a.2))
				.then(a.0.cmp(&b.0))
		});

		// Pass 2: encode, write, and drop one partition at a time.
		let mut new_pack_paths: HashSet<String> = HashSet::new();
		for (start, end) in partition_ranges(&meta, max_pack_size) {
			let mut objects: Vec<PackedObject<H>> = Vec::with_capacity(end - start);
			for (id, _, _) in &meta[start..end] {
				let (kind, data) = self.read_object_bounded(id).await?;
				objects.push(PackedObject {
					id: *id,
					kind,
					data,
				});
			}
			self
				.encode_write_bounded(&objects, max_pack_size, &mut new_pack_paths)
				.await?;
		}

		// Delete the old packs (except any a re-encode reproduced) and every loose object — all
		// now redundant with the new packs.
		let mut packs_removed = 0;
		for path in &old_packs {
			if new_pack_paths.contains(path) {
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
		self.packs.lock().await.clear();
		self.pack_bytes.lock().await.clear();

		Ok(Some(RepackReport {
			packed_objects: meta.len(),
			packs_written: new_pack_paths.len(),
			packs_removed,
			loose_removed: loose_ids.len(),
		}))
	}

	/// Encode `objects` into a pack and write it (with its `.idx`), recording its path in `out`.
	/// If the encoded pack exceeds `max_pack_size` and holds more than one object (an
	/// incompressible-data / zlib-overhead edge that the size-based partition under-estimated),
	/// split the range in half and recurse — so every written pack is ≤ `max_pack_size` unless it
	/// holds a single object whose packed form alone exceeds it (an object cannot span packs).
	async fn encode_write_bounded(
		&self,
		objects: &[PackedObject<H>],
		max_pack_size: u64,
		out: &mut HashSet<String>,
	) -> Result<(), ObjectStoreError> {
		let pack = encode_pack(objects);
		if pack.len() as u64 > max_pack_size && objects.len() > 1 {
			let mid = objects.len() / 2;
			Box::pin(self.encode_write_bounded(&objects[..mid], max_pack_size, out)).await?;
			Box::pin(self.encode_write_bounded(&objects[mid..], max_pack_size, out)).await?;
			return Ok(());
		}
		let path = pack_path_for(&pack[pack.len() - H::RAW_LEN..]);
		self.write_pack(&pack).await?;
		out.insert(path);
		Ok(())
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
