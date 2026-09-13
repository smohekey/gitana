use std::io::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use cap_std::fs::{Dir, File, OpenOptions};

use crate::{ProcessCurrentDirGuard, ProcessFileGuard};

/// Open a file that will be consumed by a child process through a retained capability.
///
/// Windows opens deny write and delete sharing so a later path-based child open cannot observe a
/// replacement. Unix children consume a private snapshot of this capability, so the ordinary open
/// is sufficient here.
pub fn open_process_file(directory: &Dir, path: &Path) -> Result<File> {
	let mut options = OpenOptions::new();
	options.read(true);
	#[cfg(windows)]
	{
		use cap_std::fs::OpenOptionsExt as _;
		use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

		options.share_mode(FILE_SHARE_READ);
	}
	directory.open_with(path, &options)
}

/// Configure `command` to consume `file` by its retained identity and return the path to pass to
/// the child.
///
/// On Unix the returned path names a mode-0400 private snapshot. This remains reopenable by programs
/// such as OpenSSH that close inherited descriptors during startup. On Windows `display_path`
/// remains the child argument; [`open_process_file`] has already pinned its target against write or
/// replacement. The returned guard must remain alive until a path-based child has opened the file;
/// retaining it until child completion is the conservative choice.
pub fn configure_process_file(
	command: &mut Command,
	file: &File,
	display_path: &Path,
) -> Result<(PathBuf, ProcessFileGuard)> {
	let retained = file.try_clone()?;

	#[cfg(all(unix, not(target_os = "fuchsia")))]
	{
		use std::io::{Seek as _, SeekFrom};
		use std::os::unix::fs::PermissionsExt as _;

		let _ = (command, display_path);
		let mut source = file.try_clone()?.into_std();
		source.seek(SeekFrom::Start(0))?;
		let mut snapshot = tempfile::NamedTempFile::new()?;
		std::io::copy(&mut source, snapshot.as_file_mut())?;
		std::fs::set_permissions(snapshot.path(), std::fs::Permissions::from_mode(0o400))?;
		let child_path = snapshot.path().to_owned();
		Ok((
			child_path,
			ProcessFileGuard {
				_file: retained,
				_snapshot: snapshot,
			},
		))
	}

	#[cfg(windows)]
	{
		let _ = command;
		let pins = pin_windows_path_chain(display_path)?;
		let final_file = pins.last().ok_or_else(|| {
			Error::new(
				std::io::ErrorKind::InvalidInput,
				"process file path has no file component",
			)
		})?;
		let visible = File::from_std(final_file.try_clone()?);
		if crate::file_identity(&visible)? != crate::file_identity(file)? {
			return Err(Error::new(
				std::io::ErrorKind::AlreadyExists,
				"process file changed before child launch",
			));
		}
		Ok((
			display_path.to_owned(),
			ProcessFileGuard {
				_file: retained,
				_pins: pins,
			},
		))
	}

	#[cfg(not(any(windows, all(unix, not(target_os = "fuchsia")))))]
	{
		let _ = (command, display_path, retained);
		Err(Error::new(
			std::io::ErrorKind::Unsupported,
			"retained process files are unsupported on this platform",
		))
	}
}

/// Configure `command` to start in the retained `directory` rather than resolving `display_path`
/// again through an untrusted ambient namespace.
///
/// The returned guard must remain alive until the command has spawned. `display_path` is used by
/// Windows because `CreateProcess` accepts only a path for the child's current directory; the
/// complete visible directory chain is pinned and the final identity is checked first.
pub fn configure_process_current_dir(
	command: &mut Command,
	directory: &Dir,
	display_path: &Path,
) -> Result<ProcessCurrentDirGuard> {
	let retained = directory.try_clone()?;

	#[cfg(all(unix, not(target_os = "fuchsia")))]
	{
		use std::os::unix::process::CommandExt as _;

		let _ = display_path;
		let child_directory = directory.try_clone()?;
		// SAFETY: the hook performs only rustix's async-signal-safe `fchdir` system call. The owned
		// directory handle is captured before spawning and remains valid in the child until exec.
		unsafe {
			command.pre_exec(move || rustix::process::fchdir(&child_directory).map_err(Error::from));
		}
		Ok(ProcessCurrentDirGuard {
			_directory: retained,
		})
	}

	#[cfg(windows)]
	{
		let pins = pin_windows_path_chain(display_path)?;
		let final_directory = pins.last().ok_or_else(|| {
			Error::new(
				std::io::ErrorKind::InvalidInput,
				"process working directory has no directory component",
			)
		})?;
		let visible = Dir::from_std_file(final_directory.try_clone()?);
		if crate::directory_identity(&visible)? != crate::directory_identity(directory)? {
			return Err(Error::new(
				std::io::ErrorKind::AlreadyExists,
				"process working directory changed before child launch",
			));
		}
		command.current_dir(display_path);
		Ok(ProcessCurrentDirGuard {
			_directory: retained,
			_pins: pins,
		})
	}

	#[cfg(not(any(windows, all(unix, not(target_os = "fuchsia")))))]
	{
		let _ = (command, display_path, retained);
		Err(Error::new(
			std::io::ErrorKind::Unsupported,
			"retained process working directories are unsupported on this platform",
		))
	}
}

#[cfg(windows)]
fn pin_windows_path_chain(path: &Path) -> Result<Vec<std::fs::File>> {
	use std::os::windows::fs::OpenOptionsExt as _;
	use std::path::{Component, PathBuf};

	use windows_sys::Win32::Storage::FileSystem::{
		FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
		FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
	};

	if !path.is_absolute() {
		return Err(Error::new(
			std::io::ErrorKind::InvalidInput,
			"process working directory must be absolute",
		));
	}

	let mut current = PathBuf::new();
	let mut pins = Vec::new();
	for component in path.components() {
		current.push(component.as_os_str());
		match component {
			Component::Prefix(_) | Component::CurDir => continue,
			Component::ParentDir => {
				return Err(Error::new(
					std::io::ErrorKind::InvalidInput,
					"process working directory must not contain parent traversal",
				));
			}
			// The volume or UNC share root cannot be retargeted like a named child. Retain it for
			// final identity validation when it is itself the cwd, but keep its sharing permissive
			// so unrelated filesystem activity is not blocked.
			Component::RootDir => {
				let mut root_options = std::fs::OpenOptions::new();
				root_options
					.access_mode(FILE_READ_ATTRIBUTES)
					.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
					.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
				pins.push(root_options.open(&current)?);
				continue;
			}
			Component::Normal(_) => {}
		}
		// Pin the named entry itself as well as the directory it resolves to. The first handle keeps
		// a junction/symlink from being retargeted; the second keeps the reached directory and proves
		// the final identity. Neither grants write or delete sharing, so reparse data and ordinary
		// directory names remain immutable until `CreateProcess` consumes the visible path.
		let mut entry_options = std::fs::OpenOptions::new();
		entry_options
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
		pins.push(entry_options.open(&current)?);
		let mut target_options = std::fs::OpenOptions::new();
		target_options
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
		pins.push(target_options.open(&current)?);
	}
	Ok(pins)
}

#[cfg(all(test, windows))]
mod windows_tests {
	use std::os::windows::fs::OpenOptionsExt as _;

	use cap_std::ambient_authority;
	use windows_sys::Win32::Storage::FileSystem::{
		FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
		FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
	};

	use super::*;

	#[test]
	fn pinned_directory_components_deny_writers_until_released() {
		let temporary = tempfile::tempdir().unwrap();
		let nested = temporary.path().join("nested");
		std::fs::create_dir(&nested).unwrap();

		let pins = pin_windows_path_chain(&nested).unwrap();
		let mut reader = std::fs::OpenOptions::new();
		reader
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
		let _reader = reader.open(&nested).expect("read access remains shareable");
		let mut writer = std::fs::OpenOptions::new();
		writer
			.access_mode(FILE_WRITE_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
		assert!(
			writer.open(&nested).is_err(),
			"a retained component must deny write access"
		);

		drop(pins);
		writer
			.open(&nested)
			.expect("releasing the process guard restores write access");
	}

	#[test]
	fn retained_process_file_denies_replacement_until_released() {
		let temporary = tempfile::tempdir().unwrap();
		let key = temporary.path().join("key");
		let replacement = temporary.path().join("replacement");
		std::fs::write(&key, b"original").unwrap();
		std::fs::write(&replacement, b"replacement").unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let retained = open_process_file(&directory, Path::new("key")).unwrap();

		assert!(
			std::fs::remove_file(&key).is_err(),
			"the retained process file must deny deletion"
		);
		drop(retained);
		std::fs::remove_file(&key).expect("releasing the retained file restores deletion access");
		std::fs::rename(&replacement, &key)
			.expect("releasing the retained file restores replacement access");
	}
}

#[cfg(all(test, unix, not(target_os = "fuchsia")))]
mod tests {
	use cap_std::ambient_authority;

	use super::*;

	#[test]
	fn child_starts_in_the_retained_directory_after_a_namespace_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let visible = temporary.path().join("visible");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&visible).unwrap();
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		std::fs::rename(&visible, &retained).unwrap();
		std::fs::create_dir(&visible).unwrap();

		let mut command = Command::new("/bin/pwd");
		let _guard = configure_process_current_dir(&mut command, &directory, &visible).unwrap();
		let output = command.output().unwrap();
		assert!(output.status.success());
		assert_eq!(
			String::from_utf8(output.stdout).unwrap().trim(),
			std::fs::canonicalize(retained).unwrap().to_str().unwrap()
		);
	}

	#[test]
	fn child_reads_the_retained_file_after_its_name_is_replaced() {
		let temporary = tempfile::tempdir().unwrap();
		std::fs::write(temporary.path().join("key"), b"original\n").unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let file = open_process_file(&directory, Path::new("key")).unwrap();
		std::fs::rename(
			temporary.path().join("key"),
			temporary.path().join("retained"),
		)
		.unwrap();
		std::fs::write(temporary.path().join("key"), b"replacement\n").unwrap();

		let mut command = Command::new("/bin/cat");
		let (path, _guard) =
			configure_process_file(&mut command, &file, &temporary.path().join("key")).unwrap();
		let output = command.arg(path).output().unwrap();
		assert!(output.status.success());
		assert_eq!(output.stdout, b"original\n");
	}
}

#[cfg(all(test, not(any(windows, all(unix, not(target_os = "fuchsia"))))))]
mod unsupported_tests {
	use cap_std::ambient_authority;

	use super::*;

	#[test]
	fn retained_working_directory_fails_closed() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let mut command = Command::new("unused");
		let error = match configure_process_current_dir(&mut command, &directory, temporary.path()) {
			Ok(_) => panic!("unsupported targets must not use an ambient working directory"),
			Err(error) => error,
		};
		assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
	}
}
