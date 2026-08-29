use std::io;
use std::path::PathBuf;

use cap_std::fs::{Dir, FileType, Metadata};
use gitana_path::{GitPath, GitPathComponent};

use crate::{DirEntry, FileKind, Meta, WorkDirFs};

/// The native [`WorkDirFs`] over a `cap_std::fs::Dir` capability — a working tree confined to one
/// directory, with no ambient authority. This is the one place gta's working-tree access is minted
/// from a real path (`CapWorkDir::from_dir(Dir::open_ambient_dir(work, …))`, at the program edge).
///
/// On unix it reports the full `stat(2)` identity (mode/uid/gid/dev/ino) and stores symlink targets
/// as raw bytes; working-tree names preserve raw bytes on byte-path unix filesystems, while macOS
/// rejects non-UTF-8 names during preflight because its native filesystems cannot create them. On
/// non-unix targets metadata degrades to size-only and a symlink's target is written as a regular
/// file — the same fallback the working tree used before this capability existed. Those platform
/// splits stay contained here so nothing above this boundary branches on the target.
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
	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()> {
		validate_representable(path)
	}

	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>> {
		let native = native_path(path)?;
		// The empty path is the work-tree root itself (e.g. a `.` pathspec normalises to `""`);
		// `symlink_metadata("")` would report it missing, so stat the directory handle directly.
		let result = if path.is_root() {
			self.dir.dir_metadata()
		} else {
			self.dir.symlink_metadata(&native)
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

	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.dir.read(native_path(path)?)
	}

	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		Ok(link_bytes(self.dir.read_link(native_path(path)?)?))
	}

	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>> {
		let entries = if path.is_root() {
			self.dir.entries()?
		} else {
			self.dir.read_dir(native_path(path)?)?
		};
		let mut out = Vec::new();
		for entry in entries {
			let entry = entry?;
			out.push(DirEntry {
				name: component_from_native(&entry.file_name())?,
				kind: kind_of_type(&entry.file_type()?),
			});
		}
		Ok(out)
	}

	fn write(&self, path: &GitPath, bytes: &[u8], executable: bool) -> io::Result<()> {
		let native = native_path(path)?;
		self.dir.write(&native, bytes)?;
		// Normalise the mode either way, so replacing an executable file with a plain one (or the
		// reverse) lands the right bit — mirroring git's checkout. A no-op where modes are absent.
		set_exec(&self.dir, &native, executable)
	}

	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()> {
		make_symlink(&self.dir, target, &native_path(path)?)
	}

	fn create_dir(&self, path: &GitPath) -> io::Result<()> {
		self.dir.create_dir(native_path(path)?)
	}

	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()> {
		self
			.dir
			.rename(native_path(from)?, &self.dir, native_path(to)?)
	}

	fn remove_file(&self, path: &GitPath) -> io::Result<()> {
		self.dir.remove_file(native_path(path)?)
	}

	fn remove_dir(&self, path: &GitPath) -> io::Result<()> {
		self.dir.remove_dir(native_path(path)?)
	}

	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()> {
		self.dir.remove_dir_all(native_path(path)?)
	}
}

#[cfg(unix)]
fn native_path(path: &GitPath) -> io::Result<PathBuf> {
	use std::ffi::OsString;
	use std::os::unix::ffi::OsStringExt as _;
	Ok(PathBuf::from(OsString::from_vec(path.as_bytes().to_vec())))
}

#[cfg(not(unix))]
fn native_path(path: &GitPath) -> io::Result<PathBuf> {
	let text = path.as_utf8().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::Unsupported,
			"Git path is not representable on this platform",
		)
	})?;
	Ok(PathBuf::from(text))
}

#[cfg(target_os = "macos")]
fn validate_representable(path: &GitPath) -> io::Result<()> {
	path.as_utf8().map(drop).ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::Unsupported,
			"Git path is not representable on this platform",
		)
	})
}

#[cfg(not(target_os = "macos"))]
fn validate_representable(path: &GitPath) -> io::Result<()> {
	native_path(path).map(drop)
}

#[cfg(unix)]
fn component_from_native(name: &std::ffi::OsStr) -> io::Result<GitPathComponent> {
	use std::os::unix::ffi::OsStrExt as _;
	GitPathComponent::from_bytes(name.as_bytes().to_vec())
		.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(not(unix))]
fn component_from_native(name: &std::ffi::OsStr) -> io::Result<GitPathComponent> {
	let text = name.to_str().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::Unsupported,
			"native path is not representable as Git bytes",
		)
	})?;
	GitPathComponent::from_utf8(text)
		.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
fn make_symlink(dir: &Dir, target: &[u8], path: &std::path::Path) -> io::Result<()> {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;
	dir.symlink(OsStr::from_bytes(target), path)
}

/// Without unix symlinks, store the target as the file's content (a lossy but round-trippable
/// fallback — the same one the working tree used before this capability).
#[cfg(not(unix))]
fn make_symlink(dir: &Dir, target: &[u8], path: &std::path::Path) -> io::Result<()> {
	dir.write(path, target)
}

#[cfg(unix)]
fn set_exec(dir: &Dir, path: &std::path::Path, executable: bool) -> io::Result<()> {
	use cap_std::fs::{Permissions, PermissionsExt};
	let mode = if executable { 0o755 } else { 0o644 };
	dir.set_permissions(path, Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_exec(_dir: &Dir, _path: &std::path::Path, _executable: bool) -> io::Result<()> {
	Ok(())
}

#[cfg(all(test, any(not(unix), target_os = "macos")))]
mod tests {
	use std::io;

	use gitana_path::GitPath;

	use super::validate_representable;

	#[test]
	fn preflight_rejects_non_utf8_git_bytes() {
		let path = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let error = validate_representable(&path).unwrap_err();
		assert_eq!(error.kind(), io::ErrorKind::Unsupported);
	}
}
