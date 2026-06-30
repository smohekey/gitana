//! In-progress rebase state — flat `.git/REBASE_*` files recording the branch being rebased, its
//! original tip (for `--abort`), the commit it is replayed onto, and the commits still to replay. The
//! state persists across `--continue` / `--skip` / `--abort` invocations. (git uses a `rebase-merge/`
//! directory; flat files avoid leaving an un-prunable empty directory that stock git would mistake
//! for an in-progress rebase.)

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::merge_state::{delete_if_present, force_write};
use crate::{Repository, RepositoryError};

// Flat `.git/REBASE_*` files (not git's `rebase-merge/` directory): the file store cannot prune an
// emptied directory, and a lingering one would make stock git think a rebase is still underway.
const HEAD_NAME: &str = "REBASE_HEAD_NAME";
const ORIG_HEAD: &str = "REBASE_ORIG_HEAD";
const ONTO: &str = "REBASE_ONTO";
const TODO: &str = "REBASE_TODO";

/// The persisted state of an in-progress rebase.
pub struct RebaseState<H: HashAlgorithm> {
	/// The branch ref being rebased (e.g. `refs/heads/feature`).
	pub head_name: String,
	/// The branch's tip before the rebase started, restored by `--abort`.
	pub orig_head: ObjectId<H>,
	/// The commit the branch is being replayed onto.
	pub onto: ObjectId<H>,
	/// The commits still to replay, oldest-first; the current step is the first.
	pub todo: Vec<ObjectId<H>>,
}

/// Record the start of a rebase.
pub(crate) async fn start_rebase<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	state: &RebaseState<H>,
) -> Result<(), RepositoryError> {
	force_write(repo, HEAD_NAME, format!("{}\n", state.head_name).as_bytes()).await?;
	force_write(repo, ORIG_HEAD, format!("{}\n", state.orig_head).as_bytes()).await?;
	force_write(repo, ONTO, format!("{}\n", state.onto).as_bytes()).await?;
	write_todo(repo, &state.todo).await
}

/// Read the in-progress rebase state, or `None` when no rebase is underway.
pub(crate) async fn rebase_state<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Option<RebaseState<H>>, RepositoryError> {
	let head_name = match repo.objects().file_store().read_path(HEAD_NAME).await {
		Ok(bytes) => utf8(bytes, HEAD_NAME)?.trim().to_owned(),
		Err(FileStoreError::NotFound) => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	Ok(Some(RebaseState {
		head_name,
		orig_head: read_oid(repo, ORIG_HEAD).await?,
		onto: read_oid(repo, ONTO).await?,
		todo: read_todo(repo).await?,
	}))
}

/// Whether a rebase is in progress.
pub(crate) async fn rebase_in_progress<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<bool, RepositoryError> {
	Ok(repo.objects().file_store().exists(HEAD_NAME).await?)
}

/// Replace the remaining-commit list (oldest-first; current step first).
pub(crate) async fn set_rebase_todo<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	todo: &[ObjectId<H>],
) -> Result<(), RepositoryError> {
	write_todo(repo, todo).await
}

/// Clear the in-progress rebase state.
pub(crate) async fn clear_rebase<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<(), RepositoryError> {
	delete_if_present(repo, HEAD_NAME).await?;
	delete_if_present(repo, ORIG_HEAD).await?;
	delete_if_present(repo, ONTO).await?;
	delete_if_present(repo, TODO).await
}

async fn write_todo<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	todo: &[ObjectId<H>],
) -> Result<(), RepositoryError> {
	let mut text = String::new();
	for oid in todo {
		text.push_str(&oid.to_hex());
		text.push('\n');
	}
	force_write(repo, TODO, text.as_bytes()).await
}

async fn read_todo<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	match repo.objects().file_store().read_path(TODO).await {
		Ok(bytes) => utf8(bytes, TODO)?
			.lines()
			.map(str::trim)
			.filter(|line| !line.is_empty())
			.map(|line| ObjectId::from_hex(line).map_err(Into::into))
			.collect(),
		Err(FileStoreError::NotFound) => Ok(Vec::new()),
		Err(error) => Err(error.into()),
	}
}

async fn read_oid<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	path: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	let bytes = repo.objects().file_store().read_path(path).await?;
	Ok(ObjectId::from_hex(utf8(bytes, path)?.trim())?)
}

fn utf8(bytes: Vec<u8>, path: &str) -> Result<String, RepositoryError> {
	String::from_utf8(bytes)
		.map_err(|_| RepositoryError::UnsupportedFormat(format!("{path} is not UTF-8")))
}
