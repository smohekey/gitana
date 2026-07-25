//! Engine → WIT error mapping.

use gitana_file_store::FileStoreError;
use gitana_object_store::ObjectStoreError;
use gitana_repository::RepositoryError;
use gitana_worktree::WorktreeError;

use crate::bindings::exports::gitana::repo::porcelain::RepoError;

/// Map engine errors onto the WIT `repo-error` surface. Anything without a more
/// precise variant is a `backend` failure.
pub(crate) fn repo_error(error: RepositoryError) -> RepoError {
	match error {
		RepositoryError::UnknownRevision(spec) => RepoError::UnknownRevision(spec),
		RepositoryError::AmbiguousRevision(hex) => RepoError::Ambiguous(hex),
		RepositoryError::InvalidRef(message) => RepoError::Invalid(message),
		RepositoryError::RefMoved { name } => RepoError::RefMoved(name),
		RepositoryError::MissingObject(id) => RepoError::NotFound(format!("missing object {id}")),
		RepositoryError::UnsupportedFormat(message) => RepoError::UnsupportedFormat(message),
		RepositoryError::Object(error) => RepoError::Invalid(error.to_string()),
		RepositoryError::FileStore(error) => file_store_error(error),
		RepositoryError::ObjectStore(error) => object_store_error(error),
		other => RepoError::Backend(other.to_string()),
	}
}

fn object_store_error(error: ObjectStoreError) -> RepoError {
	match error {
		ObjectStoreError::NotFound => RepoError::NotFound("object not found".to_owned()),
		corruption @ ObjectStoreError::Corruption { .. } => {
			RepoError::Corruption(corruption.to_string())
		}
		// A too-large input is the caller's fault, not a storage failure.
		too_large @ ObjectStoreError::TooLarge { .. } => RepoError::Invalid(too_large.to_string()),
		ObjectStoreError::Object(error) => RepoError::Invalid(error.to_string()),
		ObjectStoreError::FileStore(error) => file_store_error(error),
	}
}

fn file_store_error(error: FileStoreError) -> RepoError {
	match error {
		FileStoreError::NotFound => RepoError::NotFound("not found".to_owned()),
		other => RepoError::Backend(other.to_string()),
	}
}

/// Map a working-tree error onto the WIT `repo-error` surface. Overwrite refusals
/// (`Conflict`/`UntrackedOverwrite`) map to `conflict`, unsafe or malformed inputs to
/// `invalid`; file-store and repository errors defer to their own mappings so
/// not-found/ref-moved stay precise.
pub(crate) fn worktree_error(error: WorktreeError) -> RepoError {
	match error {
		WorktreeError::FileStore(error) => file_store_error(error),
		WorktreeError::Repository(error) => repo_error(error),
		conflict @ (WorktreeError::Conflict(_) | WorktreeError::UntrackedOverwrite(_)) => {
			RepoError::Conflict(conflict.to_string())
		}
		WorktreeError::ChecksumMismatch => RepoError::Corruption("index checksum mismatch".to_owned()),
		invalid @ (WorktreeError::Malformed(_)
		| WorktreeError::UnsafePath(_)
		| WorktreeError::PathspecMatch(_)
		| WorktreeError::EmptyPathspec
		| WorktreeError::AbsolutePathspec(_)
		| WorktreeError::IndexPathMissing(..)
		| WorktreeError::InvalidIndexSpec(_)
		// An explicit out-of-cone `add` (component `sparse-add`/`add`) and a malformed sparse config
		// value are caller/state errors, not backend failures — surface them as `invalid`.
		| WorktreeError::SparsePathExcluded(_)
		| WorktreeError::Config(_)) => RepoError::Invalid(invalid.to_string()),
		other => RepoError::Backend(other.to_string()),
	}
}
