//! In-progress merge state — the `MERGE_HEAD` and `MERGE_MSG` files git writes during a conflicted
//! merge. They record the commit being merged and the prepared message, so the merge can be
//! resolved and completed (a two-parent commit) or aborted.

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{Repository, RepositoryError};

const MERGE_HEAD: &str = "MERGE_HEAD";
const MERGE_MSG: &str = "MERGE_MSG";
const CHERRY_PICK_HEAD: &str = "CHERRY_PICK_HEAD";

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
	read_oid_file(repo, MERGE_HEAD).await
}

/// Record an in-progress cherry-pick: `CHERRY_PICK_HEAD` (the commit being picked) and `MERGE_MSG`
/// (its message, reused on completion).
pub(crate) async fn start_cherry_pick<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	commit: ObjectId<H>,
	message: &str,
) -> Result<(), RepositoryError> {
	force_write(repo, CHERRY_PICK_HEAD, format!("{commit}\n").as_bytes()).await?;
	force_write(repo, MERGE_MSG, message.as_bytes()).await
}

/// The commit recorded in `CHERRY_PICK_HEAD`, or `None` when no cherry-pick is in progress.
pub(crate) async fn cherry_pick_head<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Option<ObjectId<H>>, RepositoryError> {
	read_oid_file(repo, CHERRY_PICK_HEAD).await
}

/// Clear the in-progress cherry-pick state (`CHERRY_PICK_HEAD`, `MERGE_MSG`).
pub(crate) async fn clear_cherry_pick<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<(), RepositoryError> {
	delete_if_present(repo, CHERRY_PICK_HEAD).await?;
	delete_if_present(repo, MERGE_MSG).await
}

/// Read an object id from a state file holding one on its first line (`MERGE_HEAD`,
/// `CHERRY_PICK_HEAD`), or `None` when the file is absent.
async fn read_oid_file<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	path: &str,
) -> Result<Option<ObjectId<H>>, RepositoryError> {
	match repo.objects().file_store().read_path(path).await {
		Ok(bytes) => {
			let text = std::str::from_utf8(&bytes)
				.map_err(|_| RepositoryError::UnsupportedFormat(format!("{path} is not UTF-8")))?;
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
