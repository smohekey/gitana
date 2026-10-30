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
/// such as OpenSSH that close inherited descriptors during startup. On Windows the returned path is
/// normally the retained file's own final path, without junction aliases from `display_path`.
/// [`open_process_file`] denied write and delete sharing, so while the guard lives neither the file
/// nor any directory above it can be renamed or replaced. Keep the guard until the child has exited.
///
/// If final-path lookup reports `ERROR_PATH_NOT_FOUND` (for example, a volume without a DOS mapping),
/// `display_path` is returned after confirming it names the retained file. This compatibility
/// fallback checks identity only at configuration time; aliases in that spelling can still be
/// retargeted before child access. All other final-path lookup errors are returned unchanged.
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
		let child_path = final_path_or_fallback(final_path(file), || {
			let visible = File::from_std(std::fs::File::open(display_path)?);
			if crate::file_identity(&visible)? != crate::file_identity(file)? {
				return Err(Error::new(
					std::io::ErrorKind::AlreadyExists,
					"process file changed before child launch",
				));
			}
			Ok(display_path.to_owned())
		})?;
		Ok((child_path, ProcessFileGuard { _file: retained }))
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
/// On Unix the returned guard must remain alive until the command has spawned. On Windows
/// `CreateProcess` accepts only a path, and the child opens its own working-directory handle during
/// its startup, after `CreateProcess` returns. The child is normally started in the retained
/// directory's own final path, without junction aliases from `display_path`. The retained directory
/// must stay open until the child has exited, whether through this guard or the caller's own handle
/// to it; see [`ProcessCurrentDirGuard`].
///
/// If final-path lookup reports `ERROR_PATH_NOT_FOUND` (for example, a volume without a DOS mapping),
/// `display_path` is used after confirming it names the retained directory. This compatibility
/// fallback checks identity only at configuration time; aliases in that spelling can still be
/// retargeted before child access. All other final-path lookup errors are returned unchanged.
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
		let launch_path = final_path_or_fallback(final_path(directory), || {
			let visible = Dir::open_ambient_dir(display_path, cap_std::ambient_authority())?;
			if crate::directory_identity(&visible)? != crate::directory_identity(directory)? {
				return Err(Error::new(
					std::io::ErrorKind::AlreadyExists,
					"process working directory changed before child launch",
				));
			}
			Ok(display_path.to_owned())
		})?;
		command.current_dir(launch_path);
		Ok(ProcessCurrentDirGuard {
			_directory: retained,
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

/// Reopen a caller spelling only for the supported DOS-path lookup failure. Other failures must
/// retain their original error rather than silently weakening alias protection.
#[cfg(windows)]
fn final_path_or_fallback(
	path: Result<PathBuf>,
	fallback: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
	use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;

	match path {
		Err(error) if error.raw_os_error() == Some(ERROR_PATH_NOT_FOUND as i32) => fallback(),
		result => result,
	}
}

/// The final path of an open entry, without junctions, in the ordinary spelling when Windows would
/// resolve that to the same entry.
#[cfg(windows)]
fn final_path(entry: &impl std::os::windows::io::AsRawHandle) -> Result<PathBuf> {
	use std::os::windows::ffi::OsStringExt as _;

	use windows_sys::Win32::Storage::FileSystem::{
		FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
	};

	let mut buffer = vec![0u16; 512];
	loop {
		let capacity = u32::try_from(buffer.len()).map_err(Error::other)?;
		// SAFETY: the buffer holds `capacity` elements and the handle stays open for the call.
		let length = unsafe {
			GetFinalPathNameByHandleW(
				entry.as_raw_handle(),
				buffer.as_mut_ptr(),
				capacity,
				FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
			)
		} as usize;
		if length == 0 {
			return Err(Error::last_os_error());
		}
		if length < buffer.len() {
			buffer.truncate(length);
			let verbatim = PathBuf::from(std::ffi::OsString::from_wide(&buffer));
			return Ok(ordinary_spelling(verbatim));
		}
		// The buffer was too small; `length` is the size needed, including the terminator.
		buffer.resize(length, 0);
	}
}

/// Drop the `\\?\` prefix when Windows resolves the ordinary spelling to the same path, so children
/// receive ordinary drive-letter or UNC paths. Spellings that Windows would normalize, such as names
/// ending in a dot, keep the verbatim prefix.
#[cfg(windows)]
fn ordinary_spelling(verbatim: PathBuf) -> PathBuf {
	use std::ffi::OsString;
	use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

	let wide = verbatim.as_os_str().encode_wide().collect::<Vec<_>>();
	let unc_prefix = r"\\?\UNC\".encode_utf16().collect::<Vec<_>>();
	let local_prefix = r"\\?\".encode_utf16().collect::<Vec<_>>();
	let ordinary = if let Some(rest) = wide.strip_prefix(unc_prefix.as_slice()) {
		r"\\"
			.encode_utf16()
			.chain(rest.iter().copied())
			.collect::<Vec<_>>()
	} else if let Some(rest) = wide.strip_prefix(local_prefix.as_slice()) {
		rest.to_vec()
	} else {
		return verbatim;
	};

	let ordinary = PathBuf::from(OsString::from_wide(&ordinary));
	match std::path::absolute(&ordinary) {
		Ok(resolved) if resolved == ordinary => ordinary,
		_ => verbatim,
	}
}

#[cfg(all(test, windows))]
mod windows_tests {
	use cap_std::ambient_authority;

	use super::*;

	#[test]
	fn final_path_success_does_not_reopen_display_path() {
		let resolved = PathBuf::from(r"C:\target\key");

		let path = final_path_or_fallback(Ok(resolved.clone()), || {
			panic!("successful lookup must not reopen a mutable display path")
		})
		.unwrap();

		assert_eq!(path, resolved);
	}

	#[test]
	fn missing_dos_path_uses_the_validated_fallback() {
		use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;

		let display = PathBuf::from(r"C:\alias\key");
		let mut validated = false;

		let path = final_path_or_fallback(
			Err(Error::from_raw_os_error(ERROR_PATH_NOT_FOUND as i32)),
			|| {
				validated = true;
				Ok(display.clone())
			},
		)
		.unwrap();

		assert!(
			validated,
			"fallback must run the caller's identity validation"
		);
		assert_eq!(path, display);
	}

	#[test]
	fn other_final_path_errors_do_not_reopen_display_path() {
		use windows_sys::Win32::Foundation::{
			ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
			ERROR_NOT_ENOUGH_MEMORY,
		};

		for code in [
			ERROR_ACCESS_DENIED,
			ERROR_FILE_NOT_FOUND,
			ERROR_INVALID_HANDLE,
			ERROR_INVALID_PARAMETER,
			ERROR_NOT_ENOUGH_MEMORY,
		] {
			let error = final_path_or_fallback(Err(Error::from_raw_os_error(code as i32)), || {
				panic!("lookup error {code} must not reopen a mutable display path")
			})
			.unwrap_err();

			assert_eq!(error.raw_os_error(), Some(code as i32));
		}
	}

	#[test]
	fn fallback_validation_failure_is_preserved() {
		use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;

		let error = final_path_or_fallback(
			Err(Error::from_raw_os_error(ERROR_PATH_NOT_FOUND as i32)),
			|| {
				Err(Error::new(
					std::io::ErrorKind::AlreadyExists,
					"identity changed",
				))
			},
		)
		.unwrap_err();

		assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
		assert_eq!(error.to_string(), "identity changed");
	}

	/// Report the directory a `cmd` child started in.
	fn child_working_directory(command: &mut Command) -> PathBuf {
		let output = command.args(["/d", "/c", "cd"]).output().unwrap();
		assert!(output.status.success());
		PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
	}

	fn create_junction(link: &Path, target: &Path) {
		let output = Command::new("cmd")
			.args(["/d", "/c", "mklink", "/J"])
			.arg(link)
			.arg(target)
			.output()
			.unwrap();
		assert!(
			output.status.success(),
			"creating a junction needs no elevation"
		);
	}

	/// The ordinary spelling Windows reports for an existing entry, with no junction in it.
	fn real_path(path: &Path) -> PathBuf {
		ordinary_spelling(std::fs::canonicalize(path).unwrap())
	}

	#[test]
	fn retained_working_directory_blocks_renames_until_released() {
		let temporary = tempfile::tempdir().unwrap();
		let parent = temporary.path().join("parent");
		let nested = parent.join("nested");
		std::fs::create_dir_all(&nested).unwrap();
		let directory = Dir::open_ambient_dir(&nested, ambient_authority()).unwrap();
		let mut command = Command::new("cmd");

		let guard = configure_process_current_dir(&mut command, &directory, &nested).unwrap();
		drop(directory);

		assert!(
			std::fs::rename(&nested, parent.join("moved")).is_err(),
			"the retained working directory must not be renamed"
		);
		assert!(
			std::fs::rename(&parent, temporary.path().join("moved")).is_err(),
			"a directory above the retained working directory must not be renamed"
		);

		drop(guard);
		std::fs::rename(&nested, parent.join("moved"))
			.expect("releasing the guard restores renaming the directory");
		std::fs::rename(&parent, temporary.path().join("moved"))
			.expect("releasing the guard restores renaming its parent");
	}

	#[test]
	fn child_starts_in_the_real_directory_behind_a_junction() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let alias = temporary.path().join("alias");
		std::fs::create_dir(&target).unwrap();
		create_junction(&alias, &target);
		let directory = Dir::open_ambient_dir(&alias, ambient_authority()).unwrap();
		let mut command = Command::new("cmd");

		let _guard = configure_process_current_dir(&mut command, &directory, &alias).unwrap();

		assert_eq!(child_working_directory(&mut command), real_path(&target));
	}

	#[test]
	fn empty_and_read_only_working_directories_are_supported() {
		let temporary = tempfile::tempdir().unwrap();
		let empty = temporary.path().join("empty");
		let read_only = temporary.path().join("read-only");
		std::fs::create_dir(&empty).unwrap();
		std::fs::create_dir(&read_only).unwrap();
		let user = format!(
			"{}\\{}",
			std::env::var("USERDOMAIN").unwrap(),
			std::env::var("USERNAME").unwrap()
		);
		let restricted = Command::new("icacls")
			.arg(&read_only)
			.args(["/inheritance:r", "/grant:r"])
			.arg(format!("{user}:(OI)(CI)(RX)"))
			.output()
			.unwrap();
		assert!(restricted.status.success());
		assert!(
			std::fs::write(read_only.join("probe"), b"").is_err(),
			"the read-only fixture must deny new files"
		);

		for path in [&empty, &read_only] {
			let directory = Dir::open_ambient_dir(path, ambient_authority()).unwrap();
			let mut command = Command::new("cmd");

			let _guard = configure_process_current_dir(&mut command, &directory, path).unwrap();

			assert_eq!(child_working_directory(&mut command), real_path(path));
			assert_eq!(
				std::fs::read_dir(path).unwrap().count(),
				0,
				"configuring the child must not modify its working directory"
			);
		}
	}

	#[test]
	fn retained_process_file_is_passed_by_its_real_path() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let alias = temporary.path().join("alias");
		std::fs::create_dir(&target).unwrap();
		std::fs::write(target.join("key"), b"key").unwrap();
		create_junction(&alias, &target);
		let directory = Dir::open_ambient_dir(&alias, ambient_authority()).unwrap();
		let file = open_process_file(&directory, Path::new("key")).unwrap();
		let mut command = Command::new("cmd");

		let (path, _guard) = configure_process_file(&mut command, &file, &alias.join("key")).unwrap();

		assert_eq!(path, real_path(&target.join("key")));
	}

	#[test]
	fn ordinary_spelling_keeps_paths_windows_would_normalize() {
		assert_eq!(
			ordinary_spelling(PathBuf::from(r"\\?\C:\work\repo")),
			PathBuf::from(r"C:\work\repo")
		);
		assert_eq!(
			ordinary_spelling(PathBuf::from(r"\\?\UNC\server\share\repo")),
			PathBuf::from(r"\\server\share\repo")
		);

		assert_eq!(
			ordinary_spelling(PathBuf::from(r"\\?\C:\work\name.")),
			PathBuf::from(r"\\?\C:\work\name.")
		);
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
