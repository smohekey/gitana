use std::path::PathBuf;

/// The error type for repository discovery.
///
/// Discovery distinguishes a *genuine absence* of a repository (returned as `Ok(None)` by
/// [`try_discover`](crate::try_discover), or [`NotFound`](DiscoveryError::NotFound) by
/// [`discover`](crate::discover)) from *corrupt or inaccessible* repository metadata, which is always
/// an error. Callers rely on that split — for example to fall back to ambient configuration only when
/// there truly is no repository, while still aborting on a malformed one, as git does.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
	/// No repository was found at the starting path or any ancestor up to the filesystem root.
	#[error("not a gitana repository (or any parent up to /): {}", .start.display())]
	NotFound {
		/// The path discovery started from.
		start: PathBuf,
	},

	/// The starting path could not be accessed (e.g. it does not exist, or is unreadable).
	#[error("cannot access {}", .path.display())]
	InaccessibleStart {
		/// The starting path.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// An exact-path inspection ([`inspect_root`](crate::inspect_root)) was pointed at a path that is
	/// not itself a repository root.
	#[error("{} is not a repository root", .path.display())]
	NotWorktreeRoot {
		/// The inspected path.
		path: PathBuf,
	},

	/// A `.git` file exists but does not contain a usable `gitdir:` pointer (missing line, or an empty
	/// path), or its contents were not valid UTF-8.
	#[error("malformed .git file: {}", .path.display())]
	MalformedGitFile {
		/// The `.git` file.
		path: PathBuf,
	},

	/// A `.git` file could not be read.
	#[error("reading .git file {}", .path.display())]
	UnreadableGitFile {
		/// The `.git` file.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// The git directory a `.git` file points at is missing or inaccessible.
	#[error("git directory {} is missing or inaccessible", .path.display())]
	MissingGitDir {
		/// The resolved git-directory path.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// A `commondir` file exists but does not contain a usable path (empty), or its contents were not
	/// valid UTF-8.
	#[error("malformed commondir file: {}", .path.display())]
	MalformedCommonDir {
		/// The `commondir` file.
		path: PathBuf,
	},

	/// A `commondir` file could not be read.
	#[error("reading commondir file {}", .path.display())]
	UnreadableCommonDir {
		/// The `commondir` file.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// The common directory a `commondir` file points at is missing or inaccessible.
	#[error("common directory {} is missing or inaccessible", .path.display())]
	MissingCommonDir {
		/// The resolved common-directory path.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// The git directory's `config` (read for `core.worktree` and `core.bare`) could not be read.
	#[error("reading {}", .path.display())]
	UnreadableConfig {
		/// The `config` file.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// The git directory's `config` (or `config.worktree`) isn't valid git config, or its
	/// `core.worktree` can't name a path here.
	#[error("bad config file {}", .path.display())]
	MalformedConfig {
		/// The `config` file.
		path: PathBuf,
	},

	/// The work tree `core.worktree` names is missing or inaccessible.
	#[error("invalid path {}: the work tree core.worktree names is missing or inaccessible", .path.display())]
	MissingWorktree {
		/// The work tree's path, as `core.worktree` resolves.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},

	/// A path that exists could not be canonicalized.
	/// The work tree `core.worktree` names is the repository's git (or common) folder or inside it,
	/// where the repository's own files would be taken for work-tree files. git takes it, and a
	/// restore there overwrites its `HEAD` (probed vs git 2.55); refused here.
	#[error(
		"invalid path {}: the work tree core.worktree names is the repository's git folder or inside it, where its files would be taken for work-tree files",
		.path.display()
	)]
	WorktreeInGitDir { path: PathBuf },
	#[error("canonicalizing {}", .path.display())]
	Canonicalize {
		/// The path being canonicalized.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},
}
