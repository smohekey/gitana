use gitana_file_store::FileStoreError;
use gitana_object::{ObjectError, ObjectId};
use gitana_object_store::ObjectStoreError;

/// Errors from repository operations.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
	/// The repository's config is not a supported format (only sha256 is supported).
	#[error("unsupported repository format: {0}")]
	UnsupportedFormat(String),
	/// HEAD or a ref file could not be parsed.
	#[error("invalid ref content: {0}")]
	InvalidRef(String),
	/// A conditional ref update found a different current value than expected.
	#[error("ref moved: {name} was not at the expected value")]
	RefMoved {
		/// The ref whose update was rejected.
		name: String,
	},
	/// A referenced object does not exist.
	#[error("missing object {0}")]
	MissingObject(ObjectId),
	/// An operation not supported in the current state (e.g. committing on a
	/// detached HEAD, not yet implemented).
	#[error("unsupported operation: {0}")]
	Unsupported(String),
	/// The underlying file store failed.
	#[error("file store error: {0}")]
	FileStore(#[from] FileStoreError),
	/// The object store failed.
	#[error("object store error: {0}")]
	ObjectStore(#[from] ObjectStoreError),
	/// An object could not be decoded.
	#[error("object error: {0}")]
	Object(#[from] ObjectError),
}
