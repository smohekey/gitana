use std::io;

use gitana_path::GitPath;
use sha2::{Digest as _, Sha256};

use crate::{FileKind, Meta, WorkDirFs};

/// Which entry a working-tree name held, and as it then was, for removing that name later only
/// while it still holds the same entry, unchanged ([`WorkDirFs::remove_file_if_identity`]).
/// Recorded by [`WorkDirFs::entry_identity`], or by [`WorkDirFs::populate_new_recorded`], which
/// also records a digest of the content written, checked before removing; opaque apart from
/// [`Self::from_meta`].
///
/// Two identities are equal when they record the same entry, unchanged as far as its metadata
/// tells; a content digest, when one was recorded, is checked only on removal.
#[derive(Clone, Debug)]
pub struct WorkEntryIdentity {
	recorded: Recorded,
	content: Option<[u8; 32]>,
}

impl PartialEq for WorkEntryIdentity {
	fn eq(&self, other: &Self) -> bool {
		self.recorded == other.recorded
	}
}

impl Eq for WorkEntryIdentity {}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Recorded {
	/// The native filesystem's own identity, with the entry's length and modification time.
	#[cfg(not(target_arch = "wasm32"))]
	Native(gitana_fs_native::EntrySnapshot),
	/// An `lstat` snapshot, where the backend has no identity of its own.
	Meta {
		kind: FileKind,
		size: u64,
		mtime: (i64, u32),
		ctime: (i64, u32),
		dev: u64,
		ino: u64,
	},
}

impl WorkEntryIdentity {
	/// An identity from an `lstat` snapshot, for a backend with no identity of its own: the same
	/// entry, unchanged, while its kind, size, times, device and inode all still match.
	pub fn from_meta(meta: &Meta) -> Self {
		Self {
			recorded: Recorded::Meta {
				kind: meta.kind,
				size: meta.size,
				mtime: meta.mtime,
				ctime: meta.ctime,
				dev: meta.dev,
				ino: meta.ino,
			},
			content: None,
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn native(snapshot: gitana_fs_native::EntrySnapshot) -> Self {
		Self {
			recorded: Recorded::Native(snapshot),
			content: None,
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn as_native(&self) -> Option<gitana_fs_native::EntrySnapshot> {
		match &self.recorded {
			Recorded::Native(snapshot) => Some(*snapshot),
			Recorded::Meta { .. } => None,
		}
	}

	/// This identity with a digest of `content`, what was written (a file's bytes, a symlink's
	/// target), checked before removing.
	pub(crate) fn with_content(mut self, content: &[u8]) -> Self {
		self.content = Some(Sha256::digest(content).into());
		self
	}

	/// Whether `content` is what was written, as far as recorded: always, without a digest.
	pub(crate) fn content_matches(&self, content: &mut dyn io::Read) -> io::Result<bool> {
		let Some(expected) = self.content else {
			return Ok(true);
		};
		let mut hasher = Sha256::new();
		let mut buffer = [0u8; 64 * 1024];
		loop {
			let read = content.read(&mut buffer)?;
			if read == 0 {
				break;
			}
			hasher.update(&buffer[..read]);
		}
		Ok(<[u8; 32]>::from(hasher.finalize()) == expected)
	}
}

/// What a population entry writes (a file's bytes, a symlink's target), for its identity's digest.
pub(crate) fn population_content<'a>(entry: &crate::PopulationEntry<'a>) -> Option<&'a [u8]> {
	match entry {
		crate::PopulationEntry::Directory => None,
		crate::PopulationEntry::Regular { bytes, .. } => Some(bytes),
		crate::PopulationEntry::Symlink(target) => Some(target),
	}
}

/// [`WorkDirFs::remove_file_if_identity`]'s default: an `lstat` compared (and the content, when a
/// digest was recorded), then the name removed, leaving an instant between the two in which a
/// replacement would be removed.
pub(crate) fn remove_if_meta_matches<W: WorkDirFs + ?Sized>(
	fs: &W,
	path: &GitPath,
	expected: &WorkEntryIdentity,
) -> io::Result<bool> {
	match fs.lstat(path)? {
		Some(meta)
			if meta.kind != FileKind::Dir && WorkEntryIdentity::from_meta(&meta) == *expected =>
		{
			let content = if meta.kind == FileKind::Symlink {
				fs.read_link(path)?
			} else {
				fs.read(path)?
			};
			if !expected.content_matches(&mut content.as_slice())? {
				return Ok(false);
			}
			match fs.remove_file(path) {
				Ok(()) => Ok(true),
				Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
				Err(error) => Err(error),
			}
		}
		_ => Ok(false),
	}
}
