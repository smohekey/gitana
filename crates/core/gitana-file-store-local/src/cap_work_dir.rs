use std::io;
use std::path::PathBuf;

use cap_std::fs::{Dir, FileType, Metadata};

use crate::{DirEntry, FileKind, Meta, WorkDirFs};

/// The native [`WorkDirFs`] over a `cap_std::fs::Dir` capability — a working tree confined to one
/// directory, with no ambient authority. This is the one place gta's working-tree access is minted
/// from a real path (`CapWorkDir::from_dir(Dir::open_ambient_dir(work, …))`, at the program edge).
///
/// On unix it reports the full `stat(2)` identity (mode/uid/gid/dev/ino) and stores symlink targets
/// as raw bytes; on other targets it degrades to size-only metadata and writes a symlink's target as
/// a regular file — the same fallback the working tree used before this capability existed. That
/// unix-vs-other split is contained here, at the platform boundary, so nothing above it branches on
/// the target.
pub struct CapWorkDir {
	dir: Dir,
}

impl CapWorkDir {
	/// Build a working-tree capability over an already-opened directory.
	pub fn from_dir(dir: Dir) -> Self {
		Self { dir }
	}
}

impl WorkDirFs for CapWorkDir {
	fn lstat(&self, path: &str) -> io::Result<Option<Meta>> {
		// The empty path is the work-tree root itself (e.g. a `.` pathspec normalises to `""`);
		// `symlink_metadata("")` would report it missing, so stat the directory handle directly.
		let result = if path.is_empty() {
			self.dir.dir_metadata()
		} else {
			self.dir.symlink_metadata(path)
		};
		match result {
			Ok(md) => Ok(Some(meta_of(&md))),
			// Nothing at `path`: either no such entry, or a non-directory occupies an ancestor.
			Err(error)
				if matches!(
					error.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
				) =>
			{
				Ok(None)
			}
			// A path that leaves the capability root (a symlink component pointing outside) is not
			// reachable from this working tree, so — like git ignoring an omitted child behind an
			// untracked symlink — treat it as absent rather than aborting `status`/`diff`/reapply.
			// cap-std synthesises this as a `PermissionDenied` with no OS errno (a `Custom` error); a
			// real `EACCES` carries `raw_os_error() == Some(..)` and still propagates.
			Err(error)
				if error.kind() == io::ErrorKind::PermissionDenied && error.raw_os_error().is_none() =>
			{
				Ok(None)
			}
			Err(error) => Err(error),
		}
	}

	fn read(&self, path: &str) -> io::Result<Vec<u8>> {
		self.dir.read(path)
	}

	fn read_link(&self, path: &str) -> io::Result<Vec<u8>> {
		Ok(link_bytes(self.dir.read_link(path)?))
	}

	fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
		let entries = if path.is_empty() {
			self.dir.entries()?
		} else {
			self.dir.read_dir(path)?
		};
		let mut out = Vec::new();
		for entry in entries {
			let entry = entry?;
			out.push(DirEntry {
				name: entry.file_name().to_string_lossy().into_owned(),
				kind: kind_of_type(&entry.file_type()?),
			});
		}
		Ok(out)
	}

	fn write(&self, path: &str, bytes: &[u8], executable: bool) -> io::Result<()> {
		self.dir.write(path, bytes)?;
		// Normalise the mode either way, so replacing an executable file with a plain one (or the
		// reverse) lands the right bit — mirroring git's checkout. A no-op where modes are absent.
		set_exec(&self.dir, path, executable)
	}

	fn symlink(&self, target: &[u8], path: &str) -> io::Result<()> {
		make_symlink(&self.dir, target, path)
	}

	fn create_dir(&self, path: &str) -> io::Result<()> {
		self.dir.create_dir(path)
	}

	fn rename(&self, from: &str, to: &str) -> io::Result<()> {
		self.dir.rename(from, &self.dir, to)
	}

	fn remove_file(&self, path: &str) -> io::Result<()> {
		self.dir.remove_file(path)
	}

	fn remove_dir(&self, path: &str) -> io::Result<()> {
		self.dir.remove_dir(path)
	}

	fn remove_dir_all(&self, path: &str) -> io::Result<()> {
		self.dir.remove_dir_all(path)
	}
}

/// The git-relevant kind of a cap-std `Metadata` (an `lstat`, so a symlink stays a symlink).
fn kind_of(md: &Metadata) -> FileKind {
	if md.is_symlink() {
		FileKind::Symlink
	} else if md.is_dir() {
		FileKind::Dir
	} else if md.is_file() {
		FileKind::File
	} else {
		FileKind::Other
	}
}

/// The git-relevant kind of a cap-std `FileType` (from a directory entry, not following symlinks).
fn kind_of_type(ft: &FileType) -> FileKind {
	if ft.is_symlink() {
		FileKind::Symlink
	} else if ft.is_dir() {
		FileKind::Dir
	} else if ft.is_file() {
		FileKind::File
	} else {
		FileKind::Other
	}
}

#[cfg(unix)]
fn meta_of(md: &Metadata) -> Meta {
	use cap_std::fs::MetadataExt;
	Meta {
		kind: kind_of(md),
		size: md.len(),
		mtime: (md.mtime(), md.mtime_nsec() as u32),
		ctime: (md.ctime(), md.ctime_nsec() as u32),
		mode: md.mode(),
		dev: md.dev(),
		ino: md.ino(),
		uid: md.uid(),
		gid: md.gid(),
	}
}

/// Non-unix targets cannot report the `stat(2)` mode/identity, so only size is populated — leaving
/// the exec bit at `100644` and the stat cache always re-hashing, exactly as the pre-capability
/// `cfg(not(unix))` fallback did.
#[cfg(not(unix))]
fn meta_of(md: &Metadata) -> Meta {
	Meta {
		kind: kind_of(md),
		size: md.len(),
		mtime: (0, 0),
		ctime: (0, 0),
		mode: 0,
		dev: 0,
		ino: 0,
		uid: 0,
		gid: 0,
	}
}

#[cfg(unix)]
fn link_bytes(target: PathBuf) -> Vec<u8> {
	use std::os::unix::ffi::OsStrExt;
	target.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn link_bytes(target: PathBuf) -> Vec<u8> {
	target.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn make_symlink(dir: &Dir, target: &[u8], path: &str) -> io::Result<()> {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;
	dir.symlink(OsStr::from_bytes(target), path)
}

/// Without unix symlinks, store the target as the file's content (a lossy but round-trippable
/// fallback — the same one the working tree used before this capability).
#[cfg(not(unix))]
fn make_symlink(dir: &Dir, target: &[u8], path: &str) -> io::Result<()> {
	dir.write(path, target)
}

#[cfg(unix)]
fn set_exec(dir: &Dir, path: &str, executable: bool) -> io::Result<()> {
	use cap_std::fs::{Permissions, PermissionsExt};
	let mode = if executable { 0o755 } else { 0o644 };
	dir.set_permissions(path, Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_exec(_dir: &Dir, _path: &str, _executable: bool) -> io::Result<()> {
	Ok(())
}
