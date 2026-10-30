//! Native home selection. Explicit HOME retains the existing empty-value policy.
use std::{env, path::PathBuf};

use anyhow::Result;

pub(super) async fn discover_home() -> Result<Option<PathBuf>> {
	if let Some(home) = env::var_os("HOME") {
		return Ok((!home.is_empty()).then(|| PathBuf::from(home)));
	}
	#[cfg(windows)]
	{
		let drive = env::var_os("HOMEDRIVE");
		let path = env::var_os("HOMEPATH");
		let profile = env::var_os("USERPROFILE");
		Ok(
			tokio::task::spawn_blocking(move || {
				select_windows_home(drive, path, profile, |path| {
					!is_system_directory(path) && path.is_dir()
				})
			})
			.await?,
		)
	}
	#[cfg(not(windows))]
	Ok(None)
}

#[cfg(any(windows, test))]
fn select_windows_home(
	drive: Option<std::ffi::OsString>,
	path: Option<std::ffi::OsString>,
	profile: Option<std::ffi::OsString>,
	available: impl FnOnce(&std::path::Path) -> bool,
) -> Option<PathBuf> {
	if let Some(mut drive) = drive {
		// Git leaves HOME unset when HOMEDRIVE exists but HOMEPATH is absent.
		let path = path?;
		drive.push(path);
		let candidate = PathBuf::from(drive);
		if available(&candidate) {
			return Some(candidate);
		}
	}
	profile.filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(windows)]
fn is_system_directory(path: &std::path::Path) -> bool {
	gitana_fs_native::system_directory()
		.is_ok_and(|system| gitana_fs_native::paths_equivalent(path, &system))
}

#[cfg(test)]
mod tests {
	use super::select_windows_home;
	use std::{ffi::OsString, path::PathBuf};

	#[test]
	fn windows_home_selection() {
		let select = |drive: Option<&str>, path: Option<&str>, exists| {
			select_windows_home(
				drive.map(OsString::from),
				path.map(OsString::from),
				Some(OsString::from("profile ü")),
				|_| exists,
			)
		};
		assert_eq!(
			select(Some("C:"), Some("\\Users\\A B"), true),
			Some(PathBuf::from("C:\\Users\\A B"))
		);
		assert_eq!(
			select(Some("C:"), Some("\\missing"), false),
			Some(PathBuf::from("profile ü"))
		);
		assert_eq!(
			select(None, Some("\\ignored"), true),
			Some(PathBuf::from("profile ü"))
		);
		assert_eq!(select(Some("C:"), None, true), None);
		assert_eq!(select_windows_home(None, None, None, |_| true), None);
	}
}

#[cfg(windows)]
#[test]
fn windows_system_directory_is_excluded() {
	let system = gitana_fs_native::system_directory().unwrap();
	assert!(is_system_directory(&system));
	assert!(!is_system_directory(&system.join("drivers")));
}
