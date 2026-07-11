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

	/// A path that exists could not be canonicalized.
	#[error("canonicalizing {}", .path.display())]
	Canonicalize {
		/// The path being canonicalized.
		path: PathBuf,
		/// The underlying I/O error.
		source: std::io::Error,
	},
}
