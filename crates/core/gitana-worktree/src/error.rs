use gitana_path::{GitPath, GitPathspec};

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
	Conflict(GitPath),
	/// A two-tree merge (`switch`) was attempted while the index has unresolved conflict stages; git
	/// refuses to move `HEAD` and leave the unmerged state attached to another branch.
	#[error("you need to resolve your current index first")]
	Unmerged,
	/// A checkout would overwrite or remove an untracked working-tree file (without `--force`).
	#[error("untracked working tree file would be overwritten by checkout: {0}")]
	UntrackedOverwrite(GitPath),
	/// A tree entry's path is unsafe (traversal, `.git`, or a symlinked ancestor).
	#[error("unsafe path: {0}")]
	UnsafePath(GitPath),
	/// A pathspec resolves to an unsafe path, but is not itself a canonical [`GitPath`].
	#[error("unsafe path: {0}")]
	UnsafePathspec(GitPathspec),
	/// A textual path input could not be represented as either a path or pathspec.
	#[error("unsafe path: {0}")]
	InvalidPath(String),
	/// A pathspec matched no entries in the restore source.
	#[error("pathspec did not match any file(s): {0}")]
	PathspecMatch(GitPathspec),
	/// An explicitly-named pathspec points inside a tracked submodule (git's fatal, exit 128): the
	/// superproject cannot add a submodule's own contents.
	#[error("Pathspec '{path}' is in submodule '{submodule}'")]
	PathspecInSubmodule {
		path: GitPathspec,
		submodule: GitPath,
	},
	/// Staging an unmerged submodule (`add`) whose mount has no checked-out `HEAD` to record — git's
	/// fatal "'<path>' does not have a commit checked out": the conflict cannot be resolved.
	#[error("'{0}' does not have a commit checked out")]
	SubmoduleNoCommit(GitPath),
	/// git's fatal when a tracked submodule (gitlink) slot is occupied by a symbolic link — it refuses to
	/// treat the link as the submodule and aborts (probed vs git 2.55: `diff` → this exact message).
	#[error("expected submodule path '{0}' not to be a symbolic link")]
	SubmodulePathIsSymlink(GitPath),
	/// A tree records a gitlink (mode 160000) with an all-zero object id — not a valid cache entry. git
	/// refuses to write the index and does not switch branches (probed vs git 2.55: "cache entry has null
	/// sha1"). A non-null commit gitana does not have locally is fine (an unfetched submodule); only the
	/// null id is rejected.
	#[error("cache entry has null sha1: {0}")]
	NullGitlinkOid(GitPath),
	/// A working-tree path is a FIFO, socket, or device — git cannot hash it, so `diff` aborts rather than
	/// rendering a change (probed vs git 2.55: "'{0}': unsupported file type" / "cannot hash '{0}'").
	#[error("'{0}': unsupported file type")]
	UnsupportedFileType(GitPath),
	/// A standard excludes source (`.git/info/exclude`, or a configured/global excludes file) is a
	/// directory or is otherwise unusable — git's fatal "cannot use … as an exclude file".
	#[error("cannot use {0} as an exclude file")]
	ExcludeFile(String),
	/// A single path outside the sparse-checkout definition (git advises `--sparse`). Used by `mv` for an
	/// out-of-cone destination; `add` uses the richer [`WorktreeError::PathspecAdvisory`].
	#[error(
		"'{0}' is outside the sparse-checkout; disable or modify the sparsity rules to update it in the index"
	)]
	SparsePathExcluded(GitPath),
	/// `add` could not fully stage some pathspecs and defers git's advisory (exit non-zero) after saving
	/// the work it could stage. `sparse` are the pathspecs that matched paths outside the sparse-checkout
	/// definition (git's `--sparse` advice, in argument/discovery order); `ignored` are the reported
	/// ignored paths (git's `-f` advice, collapsed to where each rule matched and sorted lexicographically).
	/// Either or both may be non-empty; a front-end renders git's corresponding block(s).
	#[error(
		"some pathspecs could not be staged — outside the sparse-checkout: [{}]; ignored (use -f to add): [{}]",
		join_display(.sparse),
		join_display(.ignored)
	)]
	PathspecAdvisory {
		sparse: Vec<GitPathspec>,
		ignored: Vec<GitPath>,
	},
	/// An empty pathspec (`""`) was given.
	#[error("empty string is not a valid pathspec")]
	EmptyPathspec,
	/// An absolute pathspec (leading `/`) was given; only worktree-relative pathspecs are
	/// supported (unlike git, which relativises absolute paths that point inside the work tree).
	#[error("absolute pathspecs are not supported: {0}")]
	AbsolutePathspec(GitPathspec),
	/// A pathspec's magic prefix (`:(...)`) named an unknown or unsupported keyword.
	#[error("invalid pathspec magic in '{0}'")]
	InvalidPathspecMagic(GitPathspec),
	/// An index revision spec (`:<path>` / `:<n>:<path>`) named a path/stage not in the index.
	#[error("path '{0}' is not in the index{1}")]
	IndexPathMissing(GitPath, String),
	/// An index revision spec was malformed (e.g. `:/text` search, or a stage above 3).
	#[error("invalid index revision spec: '{0}'")]
	InvalidIndexSpec(GitPathspec),
	/// `rm` matched a tracked directory's contents but `-r` was not given.
	#[error("not removing '{0}' recursively without -r")]
	RecursiveRequired(GitPathspec),
	/// `rm` would lose working-tree changes not present in the index (without `-f`).
	#[error("'{0}' has local modifications (use --cached to keep the file, or -f to force removal)")]
	RmLocalModifications(GitPath),
	/// `rm` would lose changes staged in the index relative to `HEAD` (without `-f`).
	#[error(
		"'{0}' has changes staged in the index (use --cached to keep the file, or -f to force removal)"
	)]
	RmStagedChanges(GitPath),
	/// `rm` would lose index content that differs from both the working tree and `HEAD`
	/// (without `-f`).
	#[error(
		"'{0}' has staged content different from both the file and the HEAD (use -f to force removal)"
	)]
	RmStagedAndLocal(GitPath),
	/// `mv` source is not tracked (not in the index).
	#[error("source '{0}' is not under version control")]
	MvSourceUntracked(GitPathspec),
	/// `mv` source is tracked but missing from the working tree.
	#[error("bad source '{0}': does not exist in the working tree")]
	MvBadSource(GitPathspec),
	/// `mv` destination already exists and `-f` was not given.
	#[error("destination '{0}' already exists (use -f to overwrite)")]
	MvDestinationExists(GitPath),
	/// `mv` destination must be an existing directory (multiple sources, or a trailing slash).
	#[error("destination '{0}' is not a directory")]
	MvDestinationNotDir(GitPathspec),
	/// `mv` destination's parent directory does not exist.
	#[error("destination directory for '{0}' does not exist")]
	MvDestinationDirMissing(GitPath),
	/// `mv` would move a path into itself (or a subdirectory of itself).
	#[error("cannot move '{0}' into itself")]
	MvIntoSelf(GitPathspec),
	/// `mv` maps more than one source onto the same destination.
	#[error("multiple sources map to destination '{0}'")]
	MvDuplicateDestination(GitPath),
}

impl WorktreeError {
	/// Render an error at a frontend boundary without discarding the exact pathname values it carries.
	///
	/// Direct human-facing callers pass the normal [`Display`](std::fmt::Display) renderers; machine
	/// boundaries pass reversible renderers. Non-path payloads retain their ordinary error text.
	pub fn render_with_paths(
		&self,
		render_path: impl Fn(&GitPath) -> String,
		render_pathspec: impl Fn(&GitPathspec) -> String,
	) -> String {
		match self {
			Self::Conflict(path) => {
				format!(
					"checkout would overwrite local changes to {}",
					render_path(path)
				)
			}
			Self::UntrackedOverwrite(path) => format!(
				"untracked working tree file would be overwritten by checkout: {}",
				render_path(path)
			),
			Self::UnsafePath(path) => format!("unsafe path: {}", render_path(path)),
			Self::UnsafePathspec(pathspec) => {
				format!("unsafe path: {}", render_pathspec(pathspec))
			}
			Self::PathspecMatch(pathspec) => format!(
				"pathspec did not match any file(s): {}",
				render_pathspec(pathspec)
			),
			Self::PathspecInSubmodule { path, submodule } => format!(
				"Pathspec '{}' is in submodule '{}'",
				render_pathspec(path),
				render_path(submodule)
			),
			Self::SubmoduleNoCommit(path) => {
				format!("'{}' does not have a commit checked out", render_path(path))
			}
			Self::SubmodulePathIsSymlink(path) => format!(
				"expected submodule path '{}' not to be a symbolic link",
				render_path(path)
			),
			Self::NullGitlinkOid(path) => {
				format!("cache entry has null sha1: {}", render_path(path))
			}
			Self::UnsupportedFileType(path) => {
				format!("'{}': unsupported file type", render_path(path))
			}
			Self::SparsePathExcluded(path) => format!(
				"'{}' is outside the sparse-checkout; disable or modify the sparsity rules to update it in the index",
				render_path(path)
			),
			Self::PathspecAdvisory { sparse, ignored } => format!(
				"some pathspecs could not be staged — outside the sparse-checkout: [{}]; ignored (use -f to add): [{}]",
				sparse
					.iter()
					.map(&render_pathspec)
					.collect::<Vec<_>>()
					.join(", "),
				ignored
					.iter()
					.map(&render_path)
					.collect::<Vec<_>>()
					.join(", ")
			),
			Self::AbsolutePathspec(pathspec) => format!(
				"absolute pathspecs are not supported: {}",
				render_pathspec(pathspec)
			),
			Self::InvalidPathspecMagic(pathspec) => {
				format!("invalid pathspec magic in '{}'", render_pathspec(pathspec))
			}
			Self::IndexPathMissing(path, at) => {
				format!("path '{}' is not in the index{at}", render_path(path))
			}
			Self::InvalidIndexSpec(spec) => {
				format!("invalid index revision spec: '{}'", render_pathspec(spec))
			}
			Self::RecursiveRequired(pathspec) => format!(
				"not removing '{}' recursively without -r",
				render_pathspec(pathspec)
			),
			Self::RmLocalModifications(path) => format!(
				"'{}' has local modifications (use --cached to keep the file, or -f to force removal)",
				render_path(path)
			),
			Self::RmStagedChanges(path) => format!(
				"'{}' has changes staged in the index (use --cached to keep the file, or -f to force removal)",
				render_path(path)
			),
			Self::RmStagedAndLocal(path) => format!(
				"'{}' has staged content different from both the file and the HEAD (use -f to force removal)",
				render_path(path)
			),
			Self::MvSourceUntracked(pathspec) => format!(
				"source '{}' is not under version control",
				render_pathspec(pathspec)
			),
			Self::MvBadSource(pathspec) => format!(
				"bad source '{}': does not exist in the working tree",
				render_pathspec(pathspec)
			),
			Self::MvDestinationExists(path) => format!(
				"destination '{}' already exists (use -f to overwrite)",
				render_path(path)
			),
			Self::MvDestinationNotDir(pathspec) => format!(
				"destination '{}' is not a directory",
				render_pathspec(pathspec)
			),
			Self::MvDestinationDirMissing(path) => format!(
				"destination directory for '{}' does not exist",
				render_path(path)
			),
			Self::MvIntoSelf(pathspec) => {
				format!("cannot move '{}' into itself", render_pathspec(pathspec))
			}
			Self::MvDuplicateDestination(path) => format!(
				"multiple sources map to destination '{}'",
				render_path(path)
			),
			Self::Repository(error) => error.render_with_paths(&render_path, &render_pathspec),
			_ => self.to_string(),
		}
	}
}

fn join_display<T: std::fmt::Display>(values: &[T]) -> String {
	values
		.iter()
		.map(ToString::to_string)
		.collect::<Vec<_>>()
		.join(", ")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reversible_error_rendering_preserves_path_identity() {
		let raw = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let raw_error = WorktreeError::UntrackedOverwrite(raw.clone());
		let literal_error = WorktreeError::UntrackedOverwrite(literal.clone());

		assert_eq!(raw_error.to_string(), literal_error.to_string());
		let render = |error: &WorktreeError| {
			error.render_with_paths(|path| path.quote_with_affixes(b"", b""), GitPathspec::quote)
		};
		assert_ne!(render(&raw_error), render(&literal_error));

		let raw_spec = GitPathspec::from_bytes(raw.as_bytes().to_vec()).unwrap();
		let literal_spec = GitPathspec::from_bytes(literal.as_bytes().to_vec()).unwrap();
		let raw_error = WorktreeError::PathspecMatch(raw_spec);
		let literal_error = WorktreeError::PathspecMatch(literal_spec);
		assert_eq!(raw_error.to_string(), literal_error.to_string());
		assert_ne!(render(&raw_error), render(&literal_error));

		let ordinary =
			WorktreeError::InvalidIndexSpec(GitPathspec::from_utf8(":/message search").unwrap());
		assert_eq!(
			ordinary.to_string(),
			"invalid index revision spec: ':/message search'"
		);
		let raw_error =
			WorktreeError::InvalidIndexSpec(GitPathspec::from_bytes(b":raw-\xff/../x".to_vec()).unwrap());
		let literal_error =
			WorktreeError::InvalidIndexSpec(GitPathspec::from_utf8(":\"raw-\\377/../x\"").unwrap());
		assert_ne!(render(&raw_error), render(&literal_error));
	}
}
