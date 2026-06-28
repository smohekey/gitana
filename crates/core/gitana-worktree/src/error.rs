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
	/// A checkout would overwrite or remove an untracked working-tree file (without `--force`).
	#[error("untracked working tree file would be overwritten by checkout: {0}")]
	UntrackedOverwrite(String),
	/// A tree entry's path is unsafe (traversal, `.git`, or a symlinked ancestor).
	#[error("unsafe path: {0}")]
	UnsafePath(String),
	/// A pathspec matched no entries in the restore source.
	#[error("pathspec did not match any file(s): {0}")]
	PathspecMatch(String),
	/// An empty pathspec (`""`) was given.
	#[error("empty string is not a valid pathspec")]
	EmptyPathspec,
	/// An absolute pathspec (leading `/`) was given; only worktree-relative pathspecs are
	/// supported (unlike git, which relativises absolute paths that point inside the work tree).
	#[error("absolute pathspecs are not supported: {0}")]
	AbsolutePathspec(String),
	/// `rm` matched a tracked directory's contents but `-r` was not given.
	#[error("not removing '{0}' recursively without -r")]
	RecursiveRequired(String),
	/// `rm` would lose working-tree changes not present in the index (without `-f`).
	#[error("'{0}' has local modifications (use --cached to keep the file, or -f to force removal)")]
	RmLocalModifications(String),
	/// `rm` would lose changes staged in the index relative to `HEAD` (without `-f`).
	#[error(
		"'{0}' has changes staged in the index (use --cached to keep the file, or -f to force removal)"
	)]
	RmStagedChanges(String),
	/// `rm` would lose index content that differs from both the working tree and `HEAD`
	/// (without `-f`).
	#[error(
		"'{0}' has staged content different from both the file and the HEAD (use -f to force removal)"
	)]
	RmStagedAndLocal(String),
}
