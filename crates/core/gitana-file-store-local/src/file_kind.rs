/// The kind of a working-tree entry, as reported by an `lstat` that does not follow symlinks.
///
/// This is the capability-neutral equivalent of a `std::fs::FileType`: it distinguishes the three
/// kinds git cares about (regular file, directory, symlink) and folds everything else — sockets,
/// FIFOs, devices — into [`FileKind::Other`], which the working tree never tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
	/// A regular file.
	File,
	/// A directory.
	Dir,
	/// A symbolic link (the link itself, not its target).
	Symlink,
	/// Anything else (socket, FIFO, device, …).
	Other,
}

impl FileKind {
	/// Whether this is a regular file.
	pub fn is_file(self) -> bool {
		matches!(self, FileKind::File)
	}

	/// Whether this is a directory.
	pub fn is_dir(self) -> bool {
		matches!(self, FileKind::Dir)
	}

	/// Whether this is a symbolic link.
	pub fn is_symlink(self) -> bool {
		matches!(self, FileKind::Symlink)
	}
}
