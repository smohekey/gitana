use cap_std::fs::{Dir, Metadata};
use std::ffi::OsStr;
use std::time::SystemTime;

use crate::EntryIdentity;

/// An entry as it was when recorded: its identity, length and modification time. Two snapshots of
/// one name differ when the entry was replaced (even by one that reused its identity, as a freed
/// inode number often is) or rewritten in place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntrySnapshot {
	identity: EntryIdentity,
	len: u64,
	modified: Option<SystemTime>,
}

impl EntrySnapshot {
	/// Capture a snapshot from metadata obtained without following the final entry.
	pub fn from_metadata(metadata: &Metadata) -> Self {
		Self {
			identity: EntryIdentity::from_metadata(metadata),
			len: metadata.len(),
			modified: metadata
				.modified()
				.ok()
				.map(cap_std::time::SystemTime::into_std),
		}
	}

	/// The entry's identity.
	pub const fn identity(self) -> EntryIdentity {
		self.identity
	}
}

/// Capture a snapshot of a named entry without following a final symlink.
pub fn entry_snapshot(directory: &Dir, name: &OsStr) -> std::io::Result<EntrySnapshot> {
	Ok(EntrySnapshot::from_metadata(
		&directory.symlink_metadata(name)?,
	))
}
