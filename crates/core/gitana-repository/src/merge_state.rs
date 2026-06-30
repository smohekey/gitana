//! In-progress merge state — the `MERGE_HEAD` and `MERGE_MSG` files git writes during a conflicted
//! merge. They record the commit being merged and the prepared message, so the merge can be
//! resolved and completed (a two-parent commit) or aborted.

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{Repository, RepositoryError};

const MERGE_HEAD: &str = "MERGE_HEAD";
const MERGE_MSG: &str = "MERGE_MSG";

/// Record an in-progress merge: `MERGE_HEAD` (the commit being merged) and `MERGE_MSG` (the prepared
/// commit message).
pub(crate) async fn start_merge<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	merge_head: ObjectId<H>,
	message: &str,
) -> Result<(), RepositoryError> {
	force_write(repo, MERGE_HEAD, format!("{merge_head}\n").as_bytes()).await?;
	force_write(repo, MERGE_MSG, message.as_bytes()).await
}

/// The commit recorded in `MERGE_HEAD`, or `None` when no merge is in progress.
pub(crate) async fn merge_head<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Option<ObjectId<H>>, RepositoryError> {
	match repo.objects().file_store().read_path(MERGE_HEAD).await {
		Ok(bytes) => {
			let text = std::str::from_utf8(&bytes)
				.map_err(|_| RepositoryError::UnsupportedFormat("MERGE_HEAD is not UTF-8".to_owned()))?;
			Ok(Some(ObjectId::from_hex(text.trim())?))
		}
		Err(FileStoreError::NotFound) => Ok(None),
		Err(error) => Err(error.into()),
	}
}

/// The prepared merge message (`MERGE_MSG`), or `None`.
pub(crate) async fn merge_msg<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Option<String>, RepositoryError> {
	match repo.objects().file_store().read_path(MERGE_MSG).await {
		Ok(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| {
			RepositoryError::UnsupportedFormat("MERGE_MSG is not UTF-8".to_owned())
		})?)),
		Err(FileStoreError::NotFound) => Ok(None),
		Err(error) => Err(error.into()),
	}
}

/// Clear the in-progress merge state (`MERGE_HEAD`, `MERGE_MSG`).
pub(crate) async fn clear_merge<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<(), RepositoryError> {
	delete_if_present(repo, MERGE_HEAD).await?;
	delete_if_present(repo, MERGE_MSG).await
}

/// Overwrite `path` unconditionally (retrying on a concurrent change), like `write_config`.
async fn force_write<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	path: &str,
	bytes: &[u8],
) -> Result<(), RepositoryError> {
	let store = repo.objects().file_store();
	loop {
		let expected = match store.read_path_versioned(path).await {
			Ok((_, version)) => Some(version),
			Err(FileStoreError::NotFound) => None,
			Err(error) => return Err(error.into()),
		};
		match store.write_path_cas(path, bytes, expected.as_ref()).await {
			Ok(_) => return Ok(()),
			Err(FileStoreError::VersionMismatch) => continue,
			Err(error) => return Err(error.into()),
		}
	}
}

/// Delete `path` if it exists (retrying on a concurrent change); a missing path is fine.
async fn delete_if_present<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	path: &str,
) -> Result<(), RepositoryError> {
	let store = repo.objects().file_store();
	loop {
		let version = match store.read_path_versioned(path).await {
			Ok((_, version)) => version,
			Err(FileStoreError::NotFound) => return Ok(()),
			Err(error) => return Err(error.into()),
		};
		match store.delete_path(path, Some(&version)).await {
			Ok(_) => return Ok(()),
			Err(FileStoreError::VersionMismatch) => continue,
			Err(error) => return Err(error.into()),
		}
	}
}
