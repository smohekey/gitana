/// Errors from working-tree / index operations.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
	/// The index bytes were structurally invalid.
	#[error("malformed index: {0}")]
	Malformed(String),
	/// The trailing index checksum did not match its content.
	#[error("index checksum mismatch")]
	ChecksumMismatch,
	/// `.git/index.lock` already exists — another process holds the index.
	#[error("index is locked (.git/index.lock exists)")]
	IndexLocked,
	/// A filesystem error.
	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
	/// A repository (object/ref) error.
	#[error("repository error: {0}")]
	Repository(#[from] gitana_repository::RepositoryError),
	/// A checkout would overwrite uncommitted local changes (without `--force`).
	#[error("checkout would overwrite local changes to {0}")]
	Conflict(String),
	/// A tree entry's path is unsafe (traversal, `.git`, or a symlinked ancestor).
	#[error("unsafe path: {0}")]
	UnsafePath(String),
}
