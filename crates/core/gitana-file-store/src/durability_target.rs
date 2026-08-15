/// One capability-relative part of a file-store durability boundary.
///
/// A target names either one regular file, one directory namespace, or a directory tree. Paths use
/// the same git-relative spelling as [`crate::FileStore`]. The empty path names the store root for
/// [`Directory`](Self::Directory) and [`Tree`](Self::Tree); it is never a valid file target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DurabilityTarget {
	/// Flush one regular file, followed by its ancestor directory namespaces.
	File(String),
	/// Flush one directory namespace and its ancestors, without walking its children.
	Directory(String),
	/// Flush every regular file below one directory, then its directories and ancestors leaf-first.
	Tree(String),
}

impl DurabilityTarget {
	/// Construct a regular-file target.
	pub fn file(path: impl Into<String>) -> Self {
		Self::File(path.into())
	}

	/// Construct a directory-namespace target.
	pub fn directory(path: impl Into<String>) -> Self {
		Self::Directory(path.into())
	}

	/// Construct a recursive directory-tree target.
	pub fn tree(path: impl Into<String>) -> Self {
		Self::Tree(path.into())
	}

	/// The target's capability-relative path.
	pub fn path(&self) -> &str {
		match self {
			Self::File(path) | Self::Directory(path) | Self::Tree(path) => path,
		}
	}
}
