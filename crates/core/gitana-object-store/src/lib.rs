//! Git object storage: read/write git objects by id over a `GitFileStore`.
//!
//! Composition crate: it wires [`gitana_object`]'s codecs onto a
//! [`gitana_file_store::GitFileStore`] backend, scoped to one repository. Generic
//! over the backend `F` and the hash algorithm `H` — layers are wired with
//! compile-time generics (see docs/hlds/storage-layer.md). Reads try the loose object
//! first, then stored packfiles (decoded lazily and cached). Every loose read
//! recomputes the id under `H` and rejects a mismatch; objects served from a pack are
//! content-addressed by construction (`decode_pack` computes each id from its bytes).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};

use gitana_object::{
	HashAlgorithm, MAX_OBJECT_SIZE, ObjectError, ObjectId, ObjectKind, PackedObject, decode_loose,
	decode_pack, encode_loose, loose_object_path,
};
use tokio::sync::Mutex;

/// Re-exported so downstream layers name object kinds through the store layer.
pub use gitana_object::ObjectKind as Kind;

const PACK_PREFIX: &str = "objects/pack/";

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

/// A decoded pack's objects, indexed by id.
type PackIndex<H> = Arc<HashMap<ObjectId<H>, PackedObject<H>>>;

/// Git object storage layered over a file-store backend `F`, scoped to one repo, with
/// object ids under the hash algorithm `H`.
pub struct ObjectStore<F, H: HashAlgorithm> {
	files: F,
	/// Decoded packs keyed by their repository-relative path.
	packs: Mutex<HashMap<String, PackIndex<H>>>,
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

	/// Store a packfile as-is under `objects/pack/`, keyed by its trailer checksum.
	/// The pack is decoded first, so a malformed pack is never stored.
	pub async fn write_pack(&self, pack: &[u8]) -> Result<(), ObjectStoreError> {
		ensure_within(pack.len() as u64, MAX_PACK_SIZE)?;
		decode_pack::<H>(pack)?;
		let checksum = &pack[pack.len() - H::RAW_LEN..];
		let mut hex = String::with_capacity(H::RAW_LEN * 2);
		for byte in checksum {
			hex.push_str(&format!("{byte:02x}"));
		}
		let path = format!("{PACK_PREFIX}pack-{hex}.pack");
		self.files.write_path_if_absent(&path, pack).await?;
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
			Err(FileStoreError::NotFound) => match self.find_in_packs(id).await? {
				Some(object) => Ok((object.kind, object.data)),
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
		Ok(self.find_in_packs(id).await?.is_some())
	}

	async fn find_in_packs(
		&self,
		id: &ObjectId<H>,
	) -> Result<Option<PackedObject<H>>, ObjectStoreError> {
		let pack_paths = self.files.list_prefix(PACK_PREFIX).await?;
		for path in pack_paths {
			if !path.ends_with(".pack") {
				continue;
			}
			let index = self.pack_index(&path).await?;
			if let Some(object) = index.get(id) {
				return Ok(Some(object.clone()));
			}
		}
		Ok(None)
	}

	/// The decoded index for one pack path, decoding and caching it on first use.
	async fn pack_index(&self, path: &str) -> Result<PackIndex<H>, ObjectStoreError> {
		if let Some(index) = self.packs.lock().await.get(path) {
			return Ok(Arc::clone(index));
		}
		let bytes = self.files.read_path(path).await?;
		let objects = decode_pack::<H>(&bytes)?;
		let mut index = HashMap::with_capacity(objects.len());
		for object in objects {
			index.insert(object.id, object);
		}
		let index = Arc::new(index);
		self
			.packs
			.lock()
			.await
			.insert(path.to_owned(), Arc::clone(&index));
		Ok(index)
	}
}
