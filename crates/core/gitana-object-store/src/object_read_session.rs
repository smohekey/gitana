use std::collections::HashMap;

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{
	HashAlgorithm, ObjectError, ObjectId, ObjectKind, PackIndex, decode_loose, decode_object_at,
	decode_pack_index, loose_object_path,
};

use super::{ObjectBacking, ObjectStore, ObjectStoreError, index_path};

/// A point-in-time object reader that observes each pack index once.
///
/// The cache belongs only to this reader. Creating a new session reopens every index it encounters,
/// which lets a durability boundary compare physical provenance before and after its flush without
/// turning a permanent cache into authority over out-of-band repacks.
pub struct ObjectReadSession<'a, F, H: HashAlgorithm> {
	store: &'a ObjectStore<F, H>,
	indexes: HashMap<String, Option<PackIndex<H>>>,
}

impl<'a, F, H> ObjectReadSession<'a, F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	pub(super) fn new(store: &'a ObjectStore<F, H>) -> Self {
		Self {
			store,
			indexes: HashMap::new(),
		}
	}

	/// Read and content-verify an object together with its physical backing in this session's view.
	pub async fn read_object_with_backing(
		&mut self,
		id: &ObjectId<H>,
	) -> Result<(ObjectKind, Vec<u8>, ObjectBacking), ObjectStoreError> {
		let loose_path = loose_object_path(id);
		match self.store.files.read_path(&loose_path).await {
			Ok(bytes) => {
				let (kind, payload) = decode_loose(&bytes)?;
				let actual = ObjectId::<H>::compute(kind, &payload);
				if &actual != id {
					return Err(ObjectStoreError::Corruption {
						requested: id.to_hex(),
						actual: actual.to_hex(),
					});
				}
				Ok((kind, payload, ObjectBacking::Loose { path: loose_path }))
			}
			Err(FileStoreError::NotFound) => match self.store.locate(id).await? {
				Some((pack_path, meta, offset)) => {
					let bytes = self.store.pack_bytes(&pack_path).await?;
					let object = decode_object_at::<H>(&bytes, &meta.index, offset)?;
					if &object.id != id {
						return Err(ObjectStoreError::Corruption {
							requested: id.to_hex(),
							actual: object.id.to_hex(),
						});
					}

					if !self.indexes.contains_key(&pack_path) {
						let path = index_path(&pack_path);
						let index = match self.store.files.read_path(&path).await {
							Ok(bytes) => Some(decode_pack_index::<H>(&bytes)?),
							Err(FileStoreError::NotFound) => None,
							Err(error) => return Err(error.into()),
						};
						self.indexes.insert(pack_path.clone(), index);
					}
					let index = self
						.indexes
						.get(&pack_path)
						.expect("the pack index observation was inserted above");
					if index
						.as_ref()
						.is_some_and(|current| current.offset_of(id) != Some(offset))
					{
						return Err(ObjectError::MalformedPack.into());
					}
					Ok((
						object.kind,
						object.data,
						ObjectBacking::Packed {
							index: index.as_ref().map(|_| index_path(&pack_path)),
							pack: pack_path,
						},
					))
				}
				None => Err(ObjectStoreError::NotFound),
			},
			Err(other) => Err(other.into()),
		}
	}
}
