/// A git tree-entry file mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
	/// Regular non-executable file (`100644`).
	Regular,
	/// Regular executable file (`100755`).
	Executable,
	/// Symbolic link (`120000`).
	Symlink,
	/// Subdirectory / tree (`40000`).
	Directory,
}

impl FileMode {
	/// The octal mode string git writes in a tree object.
	pub fn as_str(self) -> &'static str {
		match self {
			FileMode::Regular => "100644",
			FileMode::Executable => "100755",
			FileMode::Symlink => "120000",
			FileMode::Directory => "40000",
		}
	}
}
