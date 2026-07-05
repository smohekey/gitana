use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};
use gitana_trust::ObjectSource;

use crate::{Repository, RepositoryError};

/// A repository is a trust [`ObjectSource`]: trust-root folding and object-signature verification
/// read commits, trees, and blobs through it. This is the intended dependency direction
/// (`gitana-repository` → `gitana-trust`); the trait keeps `gitana-trust` itself storage-agnostic.
impl<F, H> ObjectSource<H> for Repository<F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	type Error = RepositoryError;

	async fn read_object(&self, id: &ObjectId<H>) -> Result<(ObjectKind, Vec<u8>), RepositoryError> {
		Ok(self.objects().read_object(id).await?)
	}
}
