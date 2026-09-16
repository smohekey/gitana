use gitana_file_store::FileStoreError;
use gitana_object::ObjectError;
use gitana_object_store::ObjectStoreError;
use gitana_path::{GitPath, GitPathError, GitPathspec};

/// Errors from repository operations.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
	/// The repository's config is not a supported format (only sha256 is supported).
	#[error("unsupported repository format: {0}")]
	UnsupportedFormat(String),
	/// HEAD or a ref file could not be parsed, or a revision spec is malformed.
	#[error("invalid ref content: {0}")]
	InvalidRef(String),
	/// A revision base contains bytes that cannot form Git's textual ref/revision grammar.
	#[error("revision base is not valid UTF-8: {0}")]
	InvalidRevisionEncoding(GitPathspec),
	/// A `<revision>:<path>` suffix is not a safe canonical repository path.
	#[error("invalid tree path {path}: {reason}")]
	InvalidTreePath {
		/// Exact suffix bytes, before canonical path validation.
		path: GitPathspec,
		/// The canonical-path invariant that was violated.
		reason: GitPathError,
	},
	/// A well-formed `<revision>:<path>` suffix did not name an entry in the resolved tree.
	#[error("path '{path}' does not exist in {base}")]
	MissingTreePath {
		/// Exact repository-relative path bytes from the revision suffix.
		path: GitPath,
		/// Resolved tree-ish object id, kept as text because this error is hash-algorithm agnostic.
		base: String,
	},
	/// Tree traversal encountered a blob before reaching the requested path.
	#[error("path '{path}': '{component}' is not a directory")]
	TreePathComponentNotDirectory {
		/// Full requested canonical tree path.
		path: GitPath,
		/// Prefix that resolved to a non-tree object.
		component: GitPath,
	},
	/// A trailing slash required the resolved leaf to be a directory, but it was not.
	#[error("path '{0}' is not a directory")]
	TreePathNotDirectory(GitPathspec),
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
	/// A repository-wide history mutation is already in progress.
	#[error("repository history is being updated by another process")]
	HistoryLocked,
	/// Initial-commit publication found existing repository history.
	#[error("repository already contains history")]
	ExistingHistory,
	/// A targeted durability boundary could not observe stable ref/object storage after bounded retries.
	#[error("repository storage kept changing while making {name} durable")]
	DurabilityUnstable {
		/// The ref whose publication or deletion was being made durable.
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

impl RepositoryError {
	/// Render a repository error at a frontend boundary without discarding exact pathname bytes.
	/// Human frontends pass the ordinary path display; machine frontends pass reversible quoting.
	pub fn render_with_paths(
		&self,
		render_path: impl Fn(&GitPath) -> String,
		render_pathspec: impl Fn(&GitPathspec) -> String,
	) -> String {
		match self {
			Self::InvalidRevisionEncoding(spec) => {
				format!(
					"revision base is not valid UTF-8: {}",
					render_pathspec(spec)
				)
			}
			Self::InvalidTreePath { path, reason } => {
				format!("invalid tree path {}: {reason}", render_pathspec(path))
			}
			Self::MissingTreePath { path, base } => {
				format!("path '{}' does not exist in {base}", render_path(path))
			}
			Self::TreePathComponentNotDirectory { path, component } => format!(
				"path '{}': '{}' is not a directory",
				render_path(path),
				render_path(component)
			),
			Self::TreePathNotDirectory(path) => {
				format!("path '{}' is not a directory", render_pathspec(path))
			}
			_ => self.to_string(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn reversible(error: &RepositoryError) -> String {
		error.render_with_paths(|path| path.quote_with_affixes(b"", b""), GitPathspec::quote)
	}

	#[test]
	fn reversible_missing_tree_paths_preserve_identity() {
		let raw = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let error = |path| RepositoryError::MissingTreePath {
			path,
			base: "tree".to_owned(),
		};
		let raw = error(raw);
		let literal = error(literal);

		assert_eq!(raw.to_string(), literal.to_string());
		assert_ne!(reversible(&raw), reversible(&literal));
	}

	#[test]
	fn reversible_revision_path_errors_preserve_identity() {
		let raw_spec = GitPathspec::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal_spec = GitPathspec::from_utf8("\"raw-\\377\"").unwrap();
		let raw_path = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal_path = GitPath::from_utf8("\"raw-\\377\"").unwrap();

		let pairs = [
			(
				RepositoryError::InvalidRevisionEncoding(raw_spec.clone()),
				RepositoryError::InvalidRevisionEncoding(literal_spec.clone()),
			),
			(
				RepositoryError::InvalidTreePath {
					path: raw_spec.clone(),
					reason: GitPathError::Traversal,
				},
				RepositoryError::InvalidTreePath {
					path: literal_spec.clone(),
					reason: GitPathError::Traversal,
				},
			),
			(
				RepositoryError::TreePathComponentNotDirectory {
					path: raw_path.clone(),
					component: raw_path,
				},
				RepositoryError::TreePathComponentNotDirectory {
					path: literal_path.clone(),
					component: literal_path,
				},
			),
			(
				RepositoryError::TreePathNotDirectory(raw_spec),
				RepositoryError::TreePathNotDirectory(literal_spec),
			),
		];

		for (raw, literal) in pairs {
			assert_eq!(raw.to_string(), literal.to_string());
			assert_ne!(reversible(&raw), reversible(&literal));
		}
	}
}
