//! The `.git/shallow` file — the commit ids at a shallow repository's history boundary.
//!
//! Each line is one commit id whose parents are deliberately absent from the object store (the repo
//! was cloned or fetched with `--depth` / `--shallow-since` / `--shallow-exclude`). Reachability walks
//! must treat a boundary commit as parentless rather than as pointing at a missing object. An absent
//! file means a complete (non-shallow) repository.

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::merge_state::{delete_if_present, force_write};
use crate::{Repository, RepositoryError};

const SHALLOW: &str = "shallow";

/// The commit ids at the shallow boundary (`.git/shallow`), or an empty vec for a complete repository.
/// The order is the file's; callers that need set semantics collect into a set.
pub(crate) async fn read_shallow<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	match repo.objects().file_store().read_path(SHALLOW).await {
		Ok(bytes) => {
			let text = std::str::from_utf8(&bytes)
				.map_err(|_| RepositoryError::UnsupportedFormat("shallow is not UTF-8".to_owned()))?;
			let mut oids = Vec::new();
			for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
				oids.push(ObjectId::from_hex(line)?);
			}
			Ok(oids)
		}
		Err(FileStoreError::NotFound) => Ok(Vec::new()),
		Err(error) => Err(error.into()),
	}
}

/// Replace `.git/shallow` with `oids` (one commit id per line). An empty `oids` deletes the file — the
/// repository is then complete, and git's convention is the file's absence, not an empty file.
pub(crate) async fn write_shallow<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	oids: &[ObjectId<H>],
) -> Result<(), RepositoryError> {
	if oids.is_empty() {
		return delete_if_present(repo, SHALLOW).await;
	}
	let mut body = String::new();
	for oid in oids {
		body.push_str(&oid.to_hex());
		body.push('\n');
	}
	force_write(repo, SHALLOW, body.as_bytes()).await
}

#[cfg(test)]
mod tests {
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{ObjectKind, Sha256};
	use gitana_object_store::ObjectStore;

	use super::*;

	fn new_repo() -> Repository<MemoryFileStore, Sha256> {
		Repository::new(ObjectStore::new(MemoryFileStore::new()))
	}

	#[tokio::test]
	async fn absent_shallow_reads_as_empty() {
		let repo = new_repo();
		assert!(repo.read_shallow().await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn shallow_round_trips_and_empty_deletes_the_file() {
		let repo = new_repo();
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");

		repo.write_shallow(&[a, b]).await.unwrap();
		assert_eq!(repo.read_shallow().await.unwrap(), vec![a, b]);

		// Writing an empty boundary removes the file — the repository is complete again.
		repo.write_shallow(&[]).await.unwrap();
		assert!(repo.read_shallow().await.unwrap().is_empty());
		assert!(!repo.objects().file_store().exists(SHALLOW).await.unwrap());
	}
}
