use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, FileType, Metadata};
#[cfg(unix)]
use gitana_fs_native::rename_noreplace;
use gitana_fs_native::{
	EntryIdentity, EntrySnapshot, entry_snapshot, file_identity, remove_file_if_identity,
	remove_file_if_unchanged,
};
use gitana_path::{GitPath, GitPathComponent};

use crate::{DirEntry, FileKind, Meta, PopulationEntry, WorkDirFs, WorkEntryIdentity};

const POPULATION_TEMP_ATTEMPTS: u64 = 100;
static POPULATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// The number in a write's private file name (`.gitana-write-<pid>-<number>`), taken afresh by each.
static WRITE_NUMBER: AtomicU64 = AtomicU64::new(0);

impl WorkDirFs for CapWorkDir {
	fn uses_symlink_placeholders(&self) -> bool {
		cfg!(not(unix))
	}

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
		let path = native_path(path)?;
		let mut options = cap_std::fs::OpenOptions::new();
		options.read(true);
		// Opened without waiting, so a named pipe (or a device) where a file was expected can't hold
		// the read up for ever; then only a regular file is read.
		#[cfg(unix)]
		{
			use cap_std::fs::OpenOptionsExt;
			options.custom_flags(libc::O_NONBLOCK);
		}
		let mut file = self.dir.open_with(&path, &options)?;
		if !file.metadata()?.is_file() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("{} isn't a regular file", path.display()),
			));
		}
		let mut out = Vec::new();
		io::Read::read_to_end(&mut file, &mut out)?;
		Ok(out)
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
		// A new file renamed over the old, as git's checkout replaces a file rather than writing into
		// it: a hard link to a file elsewhere is broken, not written through (probed vs git 2.55), a
		// symlink in its place is replaced, not followed, and the file is never seen half-written.
		let name = native
			.file_name()
			.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file needs a name"))?;
		let parent = match native.parent() {
			Some(parent) if !parent.as_os_str().is_empty() => self.dir.open_dir(parent)?,
			_ => self.dir.try_clone()?,
		};
		// A private name of bounded length (not the destination's, which may be near the limit); one
		// already taken is passed over for the next, never removed: it isn't this write's.
		let mut options = cap_std::fs::OpenOptions::new();
		options.write(true).create_new(true);
		let (temporary, mut file) = loop {
			let temporary = format!(
				".gitana-write-{}-{}",
				std::process::id(),
				WRITE_NUMBER.fetch_add(1, Ordering::Relaxed)
			);
			match parent.open_with(&temporary, &options) {
				Ok(file) => break (temporary, file),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
				Err(error) => return Err(error),
			}
		};
		let identity = file_identity(&file)?;
		let written = file.write_all(bytes).and_then(|()| {
			// The mode either way, so replacing an executable file with a plain one (or the reverse)
			// lands the right bit, as git's checkout does. A no-op where modes are absent.
			set_exec_file(&file, executable)
		});
		drop(file);
		let published = written.and_then(|()| publish_written(&parent, &temporary, name, identity));
		match published {
			Ok(()) => Ok(()),
			Err(source) => match remove_failed_population_file(&parent, &temporary, identity) {
				Ok(()) => Err(source),
				Err(cleanup) => Err(io::Error::new(
					source.kind(),
					format!("{source}; removing the prepared file failed: {cleanup}"),
				)),
			},
		}
	}

	fn populate_new(&self, path: &GitPath, entry: PopulationEntry<'_>) -> io::Result<bool> {
		let native = native_path(path)?;
		let (parent, name) = population_parent(&self.dir, &native)?;
		match entry {
			PopulationEntry::Directory => match parent.create_dir(&name) {
				Ok(()) => Ok(true),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
				Err(error) => Err(error),
			},
			PopulationEntry::Regular { bytes, executable } => Ok(
				populate_regular_file(&parent, &name, |file| {
					file.write_all(bytes)?;
					set_exec_file(file, executable)
				})?
				.is_some(),
			),
			PopulationEntry::Symlink(target) => {
				make_population_symlink(&parent, target, Path::new(&name))
			}
		}
	}

	fn populate_new_recorded(
		&self,
		path: &GitPath,
		entry: PopulationEntry<'_>,
	) -> io::Result<Option<WorkEntryIdentity>> {
		let native = native_path(path)?;
		let (parent, name) = population_parent(&self.dir, &native)?;
		let content = crate::work_entry_identity::population_content(&entry);
		let snapshot = match entry {
			PopulationEntry::Directory => match parent.create_dir(&name) {
				Ok(()) => Some(entry_snapshot(&parent, &name)?),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
				Err(error) => return Err(error),
			},
			PopulationEntry::Regular { bytes, executable } => {
				populate_regular_file(&parent, &name, |file| {
					file.write_all(bytes)?;
					set_exec_file(file, executable)
				})?
			}
			PopulationEntry::Symlink(target) => {
				populate_recorded_symlink(&parent, target, Path::new(&name))?
			}
		};
		Ok(snapshot.map(|snapshot| {
			let identity = WorkEntryIdentity::native(snapshot);
			match content {
				Some(content) => identity.with_content(content),
				None => identity,
			}
		}))
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

	fn entry_identity(&self, path: &GitPath) -> io::Result<Option<WorkEntryIdentity>> {
		let native = native_path(path)?;
		let Some((parent, name)) = existing_parent(&self.dir, &native)? else {
			return Ok(None);
		};
		match entry_snapshot(&parent, &name) {
			Ok(snapshot) => Ok(Some(WorkEntryIdentity::native(snapshot))),
			Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(error) => Err(error),
		}
	}

	fn remove_file_if_identity(
		&self,
		path: &GitPath,
		expected: &WorkEntryIdentity,
	) -> io::Result<bool> {
		let Some(snapshot) = expected.as_native() else {
			return crate::work_entry_identity::remove_if_meta_matches(self, path, expected);
		};
		let native = native_path(path)?;
		let Some((parent, name)) = existing_parent(&self.dir, &native)? else {
			return Ok(false);
		};
		remove_file_if_unchanged(&parent, &name, snapshot, |content| {
			expected.content_matches(content)
		})
	}
}

/// The leaf's parent through retained no-follow directory handles, as [`population_parent`] but
/// creating nothing: `None` when a folder on the way is missing or isn't a folder (a symlink in
/// one's place included), so the leaf named isn't there.
fn existing_parent(root: &Dir, path: &Path) -> io::Result<Option<(Dir, OsString)>> {
	let mut components = path.components().peekable();
	let mut parent = root.try_clone()?;
	loop {
		let Some(Component::Normal(component)) = components.next() else {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"invalid working-tree path",
			));
		};
		if components.peek().is_none() {
			return Ok(Some((parent, component.to_owned())));
		}
		parent = match parent.open_dir_nofollow(component) {
			Ok(child) => child,
			Err(error) => {
				return match parent.symlink_metadata(component) {
					Err(missing) if missing.kind() == io::ErrorKind::NotFound => Ok(None),
					Ok(meta) if !meta.is_dir() => Ok(None),
					_ => Err(error),
				};
			}
		};
	}
}

/// Resolve the leaf parent through retained directory handles. Every accepted component is opened
/// without following a symlink, so a concurrent namespace replacement cannot redirect the final
/// exclusive create through a different directory tree.
fn population_parent(root: &Dir, path: &Path) -> io::Result<(Dir, OsString)> {
	let mut components = path.components().peekable();
	let mut parent = root.try_clone()?;
	loop {
		let Some(component) = components.next() else {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"empty population path",
			));
		};
		let Component::Normal(component) = component else {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"invalid population path component",
			));
		};
		if components.peek().is_none() {
			return Ok((parent, component.to_owned()));
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

#[cfg(unix)]
fn native_path(path: &GitPath) -> io::Result<PathBuf> {
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

#[cfg(unix)]
fn make_population_symlink(dir: &Dir, target: &[u8], path: &Path) -> io::Result<bool> {
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
fn make_symlink(dir: &Dir, target: &[u8], path: &std::path::Path) -> io::Result<()> {
	dir.write(path, target)
}

#[cfg(not(unix))]
fn make_population_symlink(dir: &Dir, target: &[u8], path: &Path) -> io::Result<bool> {
	Ok(populate_regular_file(dir, path, |file| file.write_all(target))?.is_some())
}

/// A symlink made under a private name, recorded there, then given `path` only if `path` is still
/// free: `None` when it isn't (the private one removed again).
#[cfg(unix)]
fn populate_recorded_symlink(
	dir: &Dir,
	target: &[u8],
	path: &Path,
) -> io::Result<Option<EntrySnapshot>> {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;
	match dir.symlink_metadata(path) {
		Ok(_) => return Ok(None),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error),
	}
	let mut attempts = 0;
	let temporary = loop {
		let sequence = POPULATION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
		let name = format!(".gitana-populate.{}.{}", std::process::id(), sequence);
		match dir.symlink(OsStr::from_bytes(target), &name) {
			Ok(()) => break name,
			Err(error)
				if error.kind() == io::ErrorKind::AlreadyExists
					&& attempts + 1 < POPULATION_TEMP_ATTEMPTS =>
			{
				attempts += 1;
			}
			Err(error) => return Err(error),
		}
	};
	let snapshot = entry_snapshot(dir, temporary.as_ref())?;
	match rename_noreplace(dir, temporary.as_ref(), dir, path.as_os_str()) {
		Ok(()) => Ok(Some(snapshot)),
		Err(error) => {
			let _ = remove_file_if_unchanged(dir, temporary.as_ref(), snapshot, |_| Ok(true));
			if error.kind() == io::ErrorKind::AlreadyExists {
				Ok(None)
			} else {
				Err(error)
			}
		}
	}
}

/// Without unix symlinks, the target is the file's content, recorded as any file.
#[cfg(not(unix))]
fn populate_recorded_symlink(
	dir: &Dir,
	target: &[u8],
	path: &Path,
) -> io::Result<Option<EntrySnapshot>> {
	populate_regular_file(dir, path, |file| file.write_all(target))
}

fn populate_regular_file<P: AsRef<Path>>(
	dir: &Dir,
	path: P,
	initialize: impl FnOnce(&mut cap_std::fs::File) -> io::Result<()>,
) -> io::Result<Option<EntrySnapshot>> {
	let path = path.as_ref();
	match dir.symlink_metadata(path) {
		Ok(_) => return Ok(None),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error),
	}
	let (temporary, mut file) = create_population_temp(dir)?;
	let identity = file_identity(&file)?;
	match initialize(&mut file).and_then(|()| file.metadata()) {
		Ok(written) => {
			drop(file);
			// The file as written, recorded before it has the name.
			let snapshot = EntrySnapshot::from_metadata(&written);
			match publish_population_file(dir, &temporary, path, identity) {
				Ok(()) => Ok(Some(snapshot)),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
					remove_failed_population_file(dir, &temporary, identity)?;
					Ok(None)
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

/// Put the private file `temporary` in `dir` (written, with `identity`) at `name`, renamed over what's
/// there, so the old file stays until the new one replaces it in one step (and stays if that fails):
/// only while it's still the file written, so one swapped in meanwhile is refused and the old file
/// kept; and one swapped in the instant between that check and the rename is reported, not taken
/// for the file written.
fn publish_written(
	dir: &Dir,
	temporary: &str,
	name: &OsStr,
	identity: EntryIdentity,
) -> io::Result<()> {
	if gitana_fs_native::entry_identity(dir, OsStr::new(temporary))? != identity {
		return Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			"the file being written was replaced before it was put in place",
		));
	}
	#[cfg(all(test, unix))]
	if tests::PUBLICATION_FAILS.with(std::cell::Cell::get) {
		return Err(io::Error::other("publication failed (test)"));
	}
	dir.rename(temporary, dir, name)?;
	if gitana_fs_native::entry_identity(dir, name)? != identity {
		return Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			"the file put in place was replaced as it was put there, so it isn't the file written",
		));
	}
	Ok(())
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
	path: &Path,
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
	path: &Path,
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
fn set_exec_file(file: &cap_std::fs::File, executable: bool) -> io::Result<()> {
	use cap_std::fs::{Permissions, PermissionsExt};
	let mode = if executable { 0o755 } else { 0o644 };
	file.set_permissions(Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_exec_file(_file: &cap_std::fs::File, _executable: bool) -> io::Result<()> {
	Ok(())
}

#[cfg(all(test, unix))]
mod tests {
	std::thread_local! {
		/// Makes the next publication on this thread fail at the rename, as a filesystem that can't
		/// do it would.
		pub(super) static PUBLICATION_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
	}

	/// A write whose publication fails leaves the old file as it was: it's replaced only by the
	/// rename that puts the new one in place.
	#[test]
	fn a_failed_publication_keeps_the_old_file() {
		let temp = tempfile::tempdir().unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());
		std::fs::write(temp.path().join("a.txt"), "old\n").unwrap();
		PUBLICATION_FAILS.with(|fails| fails.set(true));
		let failed = work.write(&GitPath::from_utf8("a.txt").unwrap(), b"new\n", false);
		PUBLICATION_FAILS.with(|fails| fails.set(false));
		assert!(failed.is_err());
		assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"old\n");
	}
	use std::os::unix::fs::symlink;

	use cap_std::ambient_authority;

	use super::*;

	/// A file hard-linked to one outside the work tree is replaced, as git replaces it: the outside
	/// file keeps its content, and the work-tree file stands alone.
	#[test]
	fn a_write_breaks_a_hard_link_rather_than_writing_through_it() {
		use std::os::unix::fs::MetadataExt;
		let temp = tempfile::tempdir().unwrap();
		let work_path = temp.path().join("work");
		std::fs::create_dir(&work_path).unwrap();
		let outside = temp.path().join("outside");
		std::fs::write(&outside, "outside, precious\n").unwrap();
		std::fs::hard_link(&outside, work_path.join("a.txt")).unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(&work_path, ambient_authority()).unwrap());
		work
			.write(&GitPath::from_utf8("a.txt").unwrap(), b"committed\n", false)
			.unwrap();
		assert_eq!(std::fs::read(&outside).unwrap(), b"outside, precious\n");
		assert_eq!(
			std::fs::read(work_path.join("a.txt")).unwrap(),
			b"committed\n"
		);
		assert_eq!(
			std::fs::metadata(work_path.join("a.txt")).unwrap().nlink(),
			1
		);
		let names: Vec<_> = std::fs::read_dir(&work_path)
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect();
		assert_eq!(names, ["a.txt"], "no private file left behind");
	}

	/// A private file swapped for another before it's put in place (as another process could while a
	/// large file is written) is refused: the swapped-in file isn't published, and the old file stays.
	#[test]
	fn a_swapped_private_file_is_not_published() {
		let temp = tempfile::tempdir().unwrap();
		let dir = Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap();
		std::fs::write(temp.path().join("a.txt"), "old\n").unwrap();
		let mut options = cap_std::fs::OpenOptions::new();
		options.write(true).create_new(true);
		let mut file = dir.open_with(".gitana-write-test", &options).unwrap();
		file.write_all(b"mine\n").unwrap();
		let identity = file_identity(&file).unwrap();
		drop(file);
		// Made while the written file still exists, so it can't take that file's inode.
		std::fs::write(temp.path().join("swapped"), "swapped\n").unwrap();
		std::fs::rename(
			temp.path().join("swapped"),
			temp.path().join(".gitana-write-test"),
		)
		.unwrap();
		let refused =
			publish_written(&dir, ".gitana-write-test", OsStr::new("a.txt"), identity).unwrap_err();
		assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
		assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"old\n");
	}

	/// A file already at the name a write would take for its private file isn't touched (the write
	/// takes the next), and a destination name near the length limit can still be written.
	#[test]
	fn a_write_leaves_others_files_alone_and_takes_long_names() {
		let temp = tempfile::tempdir().unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());
		let next = WRITE_NUMBER.load(Ordering::Relaxed);
		let decoy = temp
			.path()
			.join(format!(".gitana-write-{}-{next}", std::process::id()));
		std::fs::write(&decoy, "someone else's\n").unwrap();
		work
			.write(&GitPath::from_utf8("a.txt").unwrap(), b"mine\n", false)
			.unwrap();
		assert_eq!(std::fs::read(&decoy).unwrap(), b"someone else's\n");
		assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"mine\n");
		let long = "a".repeat(250);
		work
			.write(&GitPath::from_utf8(&long).unwrap(), b"long\n", false)
			.unwrap();
		assert_eq!(std::fs::read(temp.path().join(&long)).unwrap(), b"long\n");
	}

	/// A named pipe where a file was expected is refused at once, not waited on for a writer.
	#[test]
	fn a_named_pipe_read_is_refused_not_waited_on() {
		let temp = tempfile::tempdir().unwrap();
		let pipe = temp.path().join(".gitignore");
		let made = std::process::Command::new("mkfifo")
			.arg(&pipe)
			.status()
			.unwrap();
		assert!(made.success());
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());
		let (sender, receiver) = std::sync::mpsc::channel();
		std::thread::spawn(move || {
			let read = work.read(&GitPath::from_utf8(".gitignore").unwrap());
			let _ = sender.send(read.map_err(|error| error.kind()));
		});
		let Ok(read) = receiver.recv_timeout(std::time::Duration::from_secs(10)) else {
			// Let the waiting read go on, so the test can end.
			let _ = std::fs::OpenOptions::new().write(true).open(&pipe);
			panic!("the read waited on the pipe");
		};
		assert_eq!(read, Err(io::ErrorKind::InvalidInput));
	}

	#[test]
	fn population_refuses_a_symlinked_parent() {
		let temp = tempfile::tempdir().unwrap();
		std::fs::create_dir(temp.path().join("actual")).unwrap();
		symlink("actual", temp.path().join("link")).unwrap();
		let work =
			CapWorkDir::from_dir(Dir::open_ambient_dir(temp.path(), ambient_authority()).unwrap());

		let error = work
			.populate_new(
				&GitPath::from_utf8("link/file").unwrap(),
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
					&GitPath::from_utf8("nested/deeper/file").unwrap(),
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
					&GitPath::from_utf8("nested/deeper/file").unwrap(),
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
		assert!(
			populate_regular_file(&dir, "file", |file| file.write_all(b"complete"))
				.unwrap()
				.is_some()
		);
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

		let error = publish_population_file(&dir, &name, Path::new("published"), identity).unwrap_err();

		assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
		assert!(!temp.path().join("published").exists());
		assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), b"foreign");
		assert_eq!(
			std::fs::read(temp.path().join("owned-temp")).unwrap(),
			b"owned"
		);
	}
}

#[cfg(all(test, any(not(unix), target_os = "macos")))]
mod representability_tests {
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
