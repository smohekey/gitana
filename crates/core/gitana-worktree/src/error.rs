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
	/// A file-store error, e.g. reading or writing the index through the repository's store.
	#[error("file store error: {0}")]
	FileStore(#[from] gitana_file_store::FileStoreError),
	/// A repository (object/ref) error.
	#[error("repository error: {0}")]
	Repository(#[from] gitana_repository::RepositoryError),
	/// A malformed or invalid config value (e.g. a non-boolean `core.sparseCheckout`).
	#[error("config error: {0}")]
	Config(#[from] gitana_config::ConfigError),
	/// A checkout would overwrite uncommitted local changes (without `--force`).
	#[error("checkout would overwrite local changes to {0}")]
	Conflict(String),
	/// A two-tree merge (`switch`) was attempted while the index has unresolved conflict stages; git
	/// refuses to move `HEAD` and leave the unmerged state attached to another branch.
	#[error("you need to resolve your current index first")]
	Unmerged,
	/// A checkout would overwrite or remove an untracked working-tree file (without `--force`).
	#[error("untracked working tree file would be overwritten by checkout: {0}")]
	UntrackedOverwrite(String),
	/// A tree entry's path is unsafe (traversal, `.git`, or a symlinked ancestor).
	#[error("unsafe path: {0}")]
	UnsafePath(String),
	/// A pathspec matched no entries in the restore source.
	#[error("pathspec did not match any file(s): {0}")]
	PathspecMatch(String),
	/// An explicitly-named pathspec points inside a tracked submodule (git's fatal, exit 128): the
	/// superproject cannot add a submodule's own contents.
	#[error("Pathspec '{path}' is in submodule '{submodule}'")]
	PathspecInSubmodule { path: String, submodule: String },
	/// Staging an unmerged submodule (`add`) whose mount has no checked-out `HEAD` to record — git's
	/// fatal "'<path>' does not have a commit checked out": the conflict cannot be resolved.
	#[error("'{0}' does not have a commit checked out")]
	SubmoduleNoCommit(String),
	/// A standard excludes source (`.git/info/exclude`, or a configured/global excludes file) is a
	/// directory or is otherwise unusable — git's fatal "cannot use … as an exclude file".
	#[error("cannot use {0} as an exclude file")]
	ExcludeFile(String),
	/// A single path outside the sparse-checkout definition (git advises `--sparse`). Used by `mv` for an
	/// out-of-cone destination; `add` uses the richer [`WorktreeError::PathspecAdvisory`].
	#[error(
		"'{0}' is outside the sparse-checkout; disable or modify the sparsity rules to update it in the index"
	)]
	SparsePathExcluded(String),
	/// `add` could not fully stage some pathspecs and defers git's advisory (exit non-zero) after saving
	/// the work it could stage. `sparse` are the pathspecs that matched paths outside the sparse-checkout
	/// definition (git's `--sparse` advice, in argument/discovery order); `ignored` are the reported
	/// ignored paths (git's `-f` advice, collapsed to where each rule matched and sorted lexicographically).
	/// Either or both may be non-empty; a front-end renders git's corresponding block(s).
	#[error(
		"some pathspecs could not be staged — outside the sparse-checkout: [{}]; ignored (use -f to add): [{}]",
		.sparse.join(", "),
		.ignored.join(", ")
	)]
	PathspecAdvisory {
		sparse: Vec<String>,
		ignored: Vec<String>,
	},
	/// An empty pathspec (`""`) was given.
	#[error("empty string is not a valid pathspec")]
	EmptyPathspec,
	/// An absolute pathspec (leading `/`) was given; only worktree-relative pathspecs are
	/// supported (unlike git, which relativises absolute paths that point inside the work tree).
	#[error("absolute pathspecs are not supported: {0}")]
	AbsolutePathspec(String),
	/// A pathspec's magic prefix (`:(...)`) named an unknown or unsupported keyword.
	#[error("invalid pathspec magic in '{0}'")]
	InvalidPathspecMagic(String),
	/// An index revision spec (`:<path>` / `:<n>:<path>`) named a path/stage not in the index.
	#[error("path '{0}' is not in the index{1}")]
	IndexPathMissing(String, String),
	/// An index revision spec was malformed (e.g. `:/text` search, or a stage above 3).
	#[error("invalid index revision spec: ':{0}'")]
	InvalidIndexSpec(String),
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
	/// `mv` source is not tracked (not in the index).
	#[error("source '{0}' is not under version control")]
	MvSourceUntracked(String),
	/// `mv` source is tracked but missing from the working tree.
	#[error("bad source '{0}': does not exist in the working tree")]
	MvBadSource(String),
	/// `mv` destination already exists and `-f` was not given.
	#[error("destination '{0}' already exists (use -f to overwrite)")]
	MvDestinationExists(String),
	/// `mv` destination must be an existing directory (multiple sources, or a trailing slash).
	#[error("destination '{0}' is not a directory")]
	MvDestinationNotDir(String),
	/// `mv` destination's parent directory does not exist.
	#[error("destination directory for '{0}' does not exist")]
	MvDestinationDirMissing(String),
	/// `mv` would move a path into itself (or a subdirectory of itself).
	#[error("cannot move '{0}' into itself")]
	MvIntoSelf(String),
	/// `mv` maps more than one source onto the same destination.
	#[error("multiple sources map to destination '{0}'")]
	MvDuplicateDestination(String),
}
