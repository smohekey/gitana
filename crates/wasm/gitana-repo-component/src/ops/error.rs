//! Engine → WIT error mapping.

use gitana_file_store::FileStoreError;
use gitana_object_store::ObjectStoreError;
use gitana_repository::RepositoryError;

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
