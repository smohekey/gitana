use cap_fs_ext::MetadataExt as _;
use cap_std::fs::{Dir, File, Metadata};
use std::ffi::OsStr;

/// Stable identity for one no-follow filesystem entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryIdentity {
	device: u64,
	inode: u64,
}

impl EntryIdentity {
	/// Rebuild an identity previously recorded in a durable transaction journal.
	pub const fn from_parts(device: u64, inode: u64) -> Self {
		Self { device, inode }
	}

	/// Return the stable platform identity fields for durable recording.
	pub const fn parts(self) -> (u64, u64) {
		(self.device, self.inode)
	}

	/// Capture identity from metadata obtained without following the final entry.
	pub fn from_metadata(metadata: &Metadata) -> Self {
		Self {
			device: metadata.dev(),
			inode: metadata.ino(),
		}
	}
}

/// Capture the identity of a named entry without following a final symlink.
pub fn entry_identity(directory: &Dir, name: &OsStr) -> std::io::Result<EntryIdentity> {
	Ok(EntryIdentity::from_metadata(
		&directory.symlink_metadata(name)?,
	))
}

/// Capture the identity of an already-open entry.
pub fn file_identity(file: &File) -> std::io::Result<EntryIdentity> {
	Ok(EntryIdentity::from_metadata(&file.metadata()?))
}

/// Capture the identity of an already-open directory.
pub fn directory_identity(directory: &Dir) -> std::io::Result<EntryIdentity> {
	Ok(EntryIdentity::from_metadata(&directory.dir_metadata()?))
}
