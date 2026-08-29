use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, FileType, Metadata};
use gitana_fs_native::{EntryIdentity, file_identity, remove_file_if_identity};

use crate::{DirEntry, FileKind, Meta, PopulationEntry, WorkDirFs};

const POPULATION_TEMP_ATTEMPTS: u64 = 100;
static POPULATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
	fn uses_symlink_placeholders(&self) -> bool {
		cfg!(not(unix))
	}

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

	fn populate_new(&self, path: &str, entry: PopulationEntry<'_>) -> io::Result<bool> {
		let (parent, name) = population_parent(&self.dir, path)?;
		match entry {
			PopulationEntry::Directory => match parent.create_dir(name) {
				Ok(()) => Ok(true),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
				Err(error) => Err(error),
			},
			PopulationEntry::Regular { bytes, executable } => {
				populate_regular_file(&parent, name, |file| {
					file.write_all(bytes)?;
					set_exec_file(file, executable)
				})
			}
			PopulationEntry::Symlink(target) => make_population_symlink(&parent, target, name),
		}
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

/// Resolve the leaf parent through retained directory handles. Every accepted component is opened
/// without following a symlink, so a concurrent namespace replacement cannot redirect the final
/// exclusive create through a different directory tree.
fn population_parent<'a>(root: &Dir, path: &'a str) -> io::Result<(Dir, &'a str)> {
	let mut components = path.split('/').peekable();
	let mut parent = root.try_clone()?;
	loop {
		let Some(component) = components.next() else {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"empty population path",
			));
		};
		if component.is_empty() || matches!(component, "." | "..") {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"invalid population path component",
			));
		}
		if components.peek().is_none() {
			return Ok((parent, component));
		}
		parent = match parent.open_dir_nofollow(component) {
			Ok(child) => child,
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				match parent.create_dir(component) {
					Ok(()) => {}
					Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
					Err(error) => return Err(error),
				}
				parent.open_dir_nofollow(component)?
			}
			Err(error) => return Err(error),
		};
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

#[cfg(unix)]
fn make_population_symlink(dir: &Dir, target: &[u8], path: &str) -> io::Result<bool> {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;
	match dir.symlink(OsStr::from_bytes(target), path) {
		Ok(()) => Ok(true),
		Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
		Err(error) => Err(error),
	}
}

/// Without unix symlinks, store the target as the file's content (a lossy but round-trippable
/// fallback — the same one the working tree used before this capability).
#[cfg(not(unix))]
fn make_symlink(dir: &Dir, target: &[u8], path: &str) -> io::Result<()> {
	dir.write(path, target)
}

#[cfg(not(unix))]
fn make_population_symlink(dir: &Dir, target: &[u8], path: &str) -> io::Result<bool> {
	populate_regular_file(dir, path, |file| file.write_all(target))
}

fn populate_regular_file(
	dir: &Dir,
	path: &str,
	initialize: impl FnOnce(&mut cap_std::fs::File) -> io::Result<()>,
) -> io::Result<bool> {
	match dir.symlink_metadata(path) {
		Ok(_) => return Ok(false),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error),
	}
	let (temporary, mut file) = create_population_temp(dir)?;
	let identity = file_identity(&file)?;
	match initialize(&mut file) {
		Ok(()) => {
			drop(file);
			match publish_population_file(dir, &temporary, path, identity) {
				Ok(()) => Ok(true),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
					remove_failed_population_file(dir, &temporary, identity)?;
					Ok(false)
				}
				Err(source) => {
					let cleanup = remove_failed_population_file(dir, &temporary, identity);
					match cleanup {
						Ok(()) => Err(source),
						Err(cleanup) => Err(io::Error::new(
							source.kind(),
							format!("{source}; removing prepared population file failed: {cleanup}"),
						)),
					}
				}
			}
		}
		Err(source) => {
			drop(file);
			match remove_failed_population_file(dir, &temporary, identity) {
				Ok(()) => Err(source),
				Err(cleanup) => Err(io::Error::new(
					source.kind(),
					format!("{source}; removing prepared population file failed: {cleanup}"),
				)),
			}
		}
	}
}

fn create_population_temp(dir: &Dir) -> io::Result<(String, cap_std::fs::File)> {
	for _ in 0..POPULATION_TEMP_ATTEMPTS {
		let sequence = POPULATION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
		let name = format!(".gitana-populate.{}.{}", std::process::id(), sequence);
		let mut options = cap_std::fs::OpenOptions::new();
		options.write(true).create_new(true);
		match dir.open_with(&name, &options) {
			Ok(file) => return Ok((name, file)),
			Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
			Err(error) => return Err(error),
		}
	}
	Err(io::Error::new(
		io::ErrorKind::AlreadyExists,
		"could not reserve a private population file",
	))
}

#[cfg(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos"
))]
fn publish_population_file(
	dir: &Dir,
	temporary: &str,
	path: &str,
	identity: EntryIdentity,
) -> io::Result<()> {
	gitana_fs_native::rename_noreplace_if_identity(
		dir,
		temporary.as_ref(),
		identity,
		dir,
		path.as_ref(),
	)
}

#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos"
)))]
fn publish_population_file(
	dir: &Dir,
	temporary: &str,
	path: &str,
	identity: EntryIdentity,
) -> io::Result<()> {
	dir.hard_link(temporary, dir, path)?;
	match remove_file_if_identity(dir, temporary.as_ref(), identity) {
		Ok(()) => Ok(()),
		Err(error)
			if matches!(
				error.kind(),
				io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
			) =>
		{
			Ok(())
		}
		Err(error) => Err(error),
	}
}

fn remove_failed_population_file(dir: &Dir, path: &str, identity: EntryIdentity) -> io::Result<()> {
	match remove_file_if_identity(dir, path.as_ref(), identity) {
		Ok(()) => {}
		Err(error)
			if matches!(
				error.kind(),
				io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
			) => {}
		Err(error) => return Err(error),
	}
	Ok(())
}

#[cfg(unix)]
fn set_exec(dir: &Dir, path: &str, executable: bool) -> io::Result<()> {
	use cap_std::fs::{Permissions, PermissionsExt};
	let mode = if executable { 0o755 } else { 0o644 };
	dir.set_permissions(path, Permissions::from_mode(mode))
}

#[cfg(unix)]
fn set_exec_file(file: &cap_std::fs::File, executable: bool) -> io::Result<()> {
	use cap_std::fs::{Permissions, PermissionsExt};
	let mode = if executable { 0o755 } else { 0o644 };
	file.set_permissions(Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_exec(_dir: &Dir, _path: &str, _executable: bool) -> io::Result<()> {
	Ok(())
}

#[cfg(not(unix))]
fn set_exec_file(_file: &cap_std::fs::File, _executable: bool) -> io::Result<()> {
	Ok(())
}

#[cfg(all(test, unix))]
mod tests {
	use std::os::unix::fs::symlink;

	use cap_std::ambient_authority;

	use super::*;

	#[test]
	fn population_refuses_a_symlinked_parent() {
		let temp = tempfile::tempdir().unwrap();
		std::fs::create_dir(temp.path().join("actual")).unwrap();
		symlink("actual", temp.path().join("link")).unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());

		let error = work
			.populate_new(
				"link/file",
				PopulationEntry::Regular {
					bytes: b"content",
					executable: false,
				},
			)
			.unwrap_err();

		assert_ne!(error.kind(), io::ErrorKind::NotFound);
		assert!(!temp.path().join("actual/file").exists());
	}

	#[test]
	fn population_creates_missing_parents_without_replacing_a_leaf() {
		let temp = tempfile::tempdir().unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());

		assert!(
			work
				.populate_new(
					"nested/deeper/file",
					PopulationEntry::Regular {
						bytes: b"first",
						executable: false,
					},
				)
				.unwrap()
		);
		assert!(
			!work
				.populate_new(
					"nested/deeper/file",
					PopulationEntry::Regular {
						bytes: b"second",
						executable: false,
					},
				)
				.unwrap()
		);
		assert_eq!(
			std::fs::read(temp.path().join("nested/deeper/file")).unwrap(),
			b"first"
		);
	}

	#[test]
	fn failed_population_removes_its_incomplete_file() {
		let temp = tempfile::tempdir().unwrap();
		let dir = Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap();

		let error = populate_regular_file(&dir, "file", |file| {
			file.write_all(b"partial")?;
			Err(io::Error::new(
				io::ErrorKind::StorageFull,
				"injected failure",
			))
		})
		.unwrap_err();

		assert_eq!(error.kind(), io::ErrorKind::StorageFull);
		assert!(!temp.path().join("file").exists());
		assert!(populate_regular_file(&dir, "file", |file| file.write_all(b"complete")).unwrap());
		assert_eq!(
			std::fs::read(temp.path().join("file")).unwrap(),
			b"complete"
		);
	}

	#[test]
	fn failed_population_preserves_a_replacement_leaf() {
		let temp = tempfile::tempdir().unwrap();
		let dir = Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap();

		let error = populate_regular_file(&dir, "file", |file| {
			file.write_all(b"partial")?;
			std::fs::write(temp.path().join("file"), b"concurrent")?;
			Err(io::Error::new(
				io::ErrorKind::StorageFull,
				"injected failure",
			))
		})
		.unwrap_err();

		assert_eq!(error.kind(), io::ErrorKind::StorageFull);
		assert_eq!(
			std::fs::read(temp.path().join("file")).unwrap(),
			b"concurrent"
		);
	}

	#[test]
	fn failed_population_cleanup_preserves_a_replaced_private_file() {
		let temp = tempfile::tempdir().unwrap();
		let dir = Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap();
		let (name, file) = create_population_temp(&dir).unwrap();
		let identity = file_identity(&file).unwrap();
		drop(file);
		std::fs::rename(temp.path().join(&name), temp.path().join("owned-temp")).unwrap();
		std::fs::write(temp.path().join(&name), b"foreign").unwrap();

		remove_failed_population_file(&dir, &name, identity).unwrap();

		assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), b"foreign");
	}

	#[test]
	fn population_publication_rejects_a_replaced_private_file() {
		let temp = tempfile::tempdir().unwrap();
		let dir = Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap();
		let (name, mut file) = create_population_temp(&dir).unwrap();
		file.write_all(b"owned").unwrap();
		let identity = file_identity(&file).unwrap();
		drop(file);
		std::fs::rename(temp.path().join(&name), temp.path().join("owned-temp")).unwrap();
		std::fs::write(temp.path().join(&name), b"foreign").unwrap();

		let error = publish_population_file(&dir, &name, "published", identity).unwrap_err();

		assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
		assert!(!temp.path().join("published").exists());
		assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), b"foreign");
		assert_eq!(
			std::fs::read(temp.path().join("owned-temp")).unwrap(),
			b"owned"
		);
	}
}
