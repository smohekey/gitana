//! Reading `stat(2)` data and git modes from working-tree files.

use std::fs::Metadata;
use std::path::Path;

use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};

use crate::Stat;

/// Hash a working-tree file into a blob id (without writing it) with its git mode.
/// `None` for anything that is neither a regular file nor a symlink.
pub(crate) fn blob_of<H: HashAlgorithm>(
	full: &Path,
	meta: &Metadata,
) -> std::io::Result<Option<(ObjectId<H>, u32)>> {
	if meta.is_symlink() {
		let target = std::fs::read_link(full)?;
		Ok(Some((
			ObjectId::<H>::compute(ObjectKind::Blob, path_bytes(&target)),
			0o120000,
		)))
	} else if meta.is_file() {
		let content = std::fs::read(full)?;
		Ok(Some((
			ObjectId::<H>::compute(ObjectKind::Blob, &content),
			file_mode(meta),
		)))
	} else {
		Ok(None)
	}
}

/// The bytes a symlink stores as its blob content: the link target path.
#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> &[u8] {
	use std::os::unix::ffi::OsStrExt;
	path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
pub(crate) fn path_bytes(path: &Path) -> &[u8] {
	path.to_str().unwrap_or_default().as_bytes()
}

/// The git mode for a regular file (`100755` if any execute bit is set).
#[cfg(unix)]
pub(crate) fn file_mode(meta: &Metadata) -> u32 {
	use std::os::unix::fs::PermissionsExt;
	if meta.permissions().mode() & 0o111 != 0 {
		0o100755
	} else {
		0o100644
	}
}

#[cfg(not(unix))]
pub(crate) fn file_mode(_meta: &Metadata) -> u32 {
	0o100644
}

/// The git mode for an `lstat`ed entry (symlink, executable, or regular file).
pub(crate) fn mode_of(meta: &Metadata) -> u32 {
	if meta.is_symlink() {
		0o120000
	} else {
		file_mode(meta)
	}
}

/// The index stat cache for a working-tree file.
#[cfg(unix)]
pub(crate) fn stat_of(meta: &Metadata) -> Stat {
	use std::os::unix::fs::MetadataExt;
	Stat {
		ctime_sec: meta.ctime() as u32,
		ctime_nsec: meta.ctime_nsec() as u32,
		mtime_sec: meta.mtime() as u32,
		mtime_nsec: meta.mtime_nsec() as u32,
		dev: meta.dev() as u32,
		ino: meta.ino() as u32,
		uid: meta.uid(),
		gid: meta.gid(),
		size: meta.size() as u32,
	}
}

#[cfg(not(unix))]
pub(crate) fn stat_of(meta: &Metadata) -> Stat {
	Stat {
		size: meta.len() as u32,
		..Stat::default()
	}
}
