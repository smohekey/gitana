use gitana_file_store::FileStoreError;
use gitana_object::ObjectError;
use gitana_object_store::ObjectStoreError;

/// Errors from repository operations.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
	/// The repository's config is not a supported format (only sha256 is supported).
	#[error("unsupported repository format: {0}")]
	UnsupportedFormat(String),
	/// HEAD or a ref file could not be parsed, or a revision spec is malformed.
	#[error("invalid ref content: {0}")]
	InvalidRef(String),
	/// A requested tree contains an invalid path, mode, or path conflict.
	#[error("invalid tree: {0}")]
	InvalidTree(String),
	/// A (well-formed) revision spec did not resolve to any object.
	#[error("unknown revision: {0}")]
	UnknownRevision(String),
	/// An abbreviated object id matches more than one object.
	#[error("ambiguous abbreviation: {0}")]
	AmbiguousRevision(String),
	/// A conditional ref update found a different current value than expected.
	#[error("ref moved: {name} was not at the expected value")]
	RefMoved {
		/// The ref whose update was rejected.
		name: String,
	},
	/// A ref transaction could not acquire a ref's `<ref>.lock` — another writer holds it.
	#[error("ref locked: {name} is being updated by another process")]
	RefLocked {
		/// The ref whose lock was contended.
		name: String,
	},
	/// An owned repository mutation task could not be joined.
	#[error("retained repository task failed: {0}")]
	RetainedTask(String),
	/// A referenced object does not exist (the hex id is recorded for diagnostics).
	#[error("missing object {0}")]
	MissingObject(String),
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
