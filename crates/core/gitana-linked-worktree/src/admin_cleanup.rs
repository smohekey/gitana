//! Capability-relative deletion helpers shared by removal and prepared-create recovery.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_std::fs::Dir;

static REMOVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_TRASH_NAMES: u32 = 4096;

/// Open the exact directory named by `path` without following its final component.
pub(crate) fn open_directory_nofollow(path: &Path) -> std::io::Result<Dir> {
	open_directory_file_nofollow(path).map(Dir::from_std_file)
}

fn open_directory_file_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
	let normalized = normalize_directory_path(path)?;
	open_directory_nofollow_impl(&normalized)
}

fn normalize_directory_path(path: &Path) -> std::io::Result<PathBuf> {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		if matches!(component, Component::CurDir | Component::ParentDir) {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"directory path contains a relative traversal component",
			));
		}
		normalized.push(component);
	}
	if normalized.as_os_str().is_empty() {
		return Err(std::io::ErrorKind::InvalidInput.into());
	}
	Ok(normalized)
}

#[cfg(unix)]
fn open_directory_nofollow_impl(path: &Path) -> std::io::Result<std::fs::File> {
	use std::os::unix::fs::OpenOptionsExt;

	std::fs::OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
		.open(path)
}

#[cfg(windows)]
fn open_directory_nofollow_impl(path: &Path) -> std::io::Result<std::fs::File> {
	use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
	use windows_sys::Win32::Storage::FileSystem::{
		FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
	};

	let file = std::fs::OpenOptions::new()
		.read(true)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
		.open(path)?;
	let metadata = file.metadata()?;
	if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"path is not a no-follow directory",
		));
	}
	Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_directory_nofollow_impl(path: &Path) -> std::io::Result<std::fs::File> {
	let metadata = std::fs::symlink_metadata(path)?;
	if !metadata.is_dir() || metadata.file_type().is_symlink() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"path is not a no-follow directory",
		));
	}
	std::fs::File::open(path)
}

/// Remove an exact directory tree without following a replacement at its leaf.
pub(crate) fn remove_directory_tree(path: &Path) -> std::io::Result<()> {
	let normalized = normalize_directory_path(path)?;
	let parent = normalized
		.parent()
		.ok_or(std::io::ErrorKind::InvalidInput)?;
	remove_quarantined(&normalized, parent, true)
}

/// Remove one exact empty directory without following a replacement at its leaf.
pub(crate) fn remove_empty_directory(path: &Path) -> std::io::Result<()> {
	let normalized = normalize_directory_path(path)?;
	let parent = normalized
		.parent()
		.ok_or(std::io::ErrorKind::InvalidInput)?;
	remove_quarantined(&normalized, parent, false)
}

/// Atomically de-register `admin`, then delete the moved bytes best-effort outside `worktrees/`.
pub(crate) fn deregister_admin(admin: &Path) -> std::io::Result<()> {
	let worktrees = admin.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
	let common = worktrees.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
	let Some(quarantined) = quarantine_directory(admin, common)? else {
		return Ok(());
	};
	let _ = quarantined.directory.remove_open_dir_all();
	Ok(())
}

struct QuarantinedDirectory {
	original: PathBuf,
	trash: PathBuf,
	directory: Dir,
}

fn remove_quarantined(path: &Path, trash_parent: &Path, recursive: bool) -> std::io::Result<()> {
	let Some(quarantined) = quarantine_directory(path, trash_parent)? else {
		return Ok(());
	};
	let QuarantinedDirectory {
		original,
		trash,
		directory,
	} = quarantined;
	let result = if recursive {
		directory.remove_open_dir_all()
	} else {
		directory.remove_open_dir()
	};
	if let Err(error) = result {
		if path_absent(&original) {
			let _ = std::fs::rename(&trash, &original);
		}
		return Err(error);
	}
	Ok(())
}

/// Move the exact opened directory to a private sibling before any recursive pathname operation.
///
/// This matters on Windows, where cap-std must close a directory handle before recursively removing
/// it. The live user-controlled name is no longer involved by then. Identity verification also
/// catches a directory replacement between the no-follow open and the rename.
fn quarantine_directory(
	path: &Path,
	trash_parent: &Path,
) -> std::io::Result<Option<QuarantinedDirectory>> {
	let original = normalize_directory_path(path)?;
	let opened = match open_directory_file_nofollow(&original) {
		Ok(opened) => opened,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error),
	};
	for _ in 0..MAX_TRASH_NAMES {
		let sequence = REMOVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		let trash = trash_parent.join(format!(
			".gitana-removing.{}.{}",
			std::process::id(),
			sequence
		));
		match std::fs::symlink_metadata(&trash) {
			Ok(_) => continue,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error),
		}

		match std::fs::rename(&original, &trash) {
			Ok(()) => {
				let moved = match open_directory_file_nofollow(&trash) {
					Ok(moved) => moved,
					Err(error) => {
						drop(opened);
						restore_quarantine(&trash, &original);
						return Err(error);
					}
				};
				let same_identity = match same_directory_identity(&opened, &moved) {
					Ok(same_identity) => same_identity,
					Err(error) => {
						drop(moved);
						drop(opened);
						restore_quarantine(&trash, &original);
						return Err(error);
					}
				};
				if !same_identity {
					drop(moved);
					drop(opened);
					restore_quarantine(&trash, &original);
					return Err(std::io::Error::new(
						std::io::ErrorKind::InvalidData,
						"directory identity changed while it was being quarantined",
					));
				}
				drop(opened);
				return Ok(Some(QuarantinedDirectory {
					original,
					trash,
					directory: Dir::from_std_file(moved),
				}));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(error) => match std::fs::symlink_metadata(&trash) {
				Ok(_) => continue,
				Err(stat_error) if stat_error.kind() == std::io::ErrorKind::NotFound => {
					return Err(error);
				}
				Err(stat_error) => return Err(stat_error),
			},
		}
	}

	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"exhausted private directory quarantine names",
	))
}

fn restore_quarantine(trash: &Path, original: &Path) {
	if path_absent(original) {
		let _ = std::fs::rename(trash, original);
	}
}

#[cfg(unix)]
fn same_directory_identity(left: &std::fs::File, right: &std::fs::File) -> std::io::Result<bool> {
	use std::os::unix::fs::MetadataExt;
	let left = left.metadata()?;
	let right = right.metadata()?;
	Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(windows)]
fn same_directory_identity(left: &std::fs::File, right: &std::fs::File) -> std::io::Result<bool> {
	fn identity(file: &std::fs::File) -> std::io::Result<(u64, u64)> {
		let information = winx::winapi_util::file::information(file)?;
		Ok((information.volume_serial_number(), information.file_index()))
	}

	Ok(identity(left)? == identity(right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_directory_identity(_left: &std::fs::File, _right: &std::fs::File) -> std::io::Result<bool> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"stable directory identity is unavailable on this platform",
	))
}

/// Whether `path` is confirmed absent by a no-follow stat.
pub(crate) fn path_absent(path: &Path) -> bool {
	matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
	#[cfg(unix)]
	use std::os::unix::fs::symlink;

	use super::*;

	#[cfg(unix)]
	#[test]
	fn recursive_cleanup_does_not_follow_a_replacement_symlink() {
		let root = tempfile::tempdir().unwrap();
		let owned = root.path().join("owned");
		let foreign = root.path().join("foreign");
		std::fs::create_dir(&owned).unwrap();
		std::fs::create_dir(&foreign).unwrap();
		std::fs::write(foreign.join("keep"), b"foreign").unwrap();
		std::fs::remove_dir(&owned).unwrap();
		symlink(&foreign, &owned).unwrap();

		let trailing_separator = PathBuf::from(format!("{}/", owned.display()));
		remove_directory_tree(&trailing_separator)
			.expect_err("a symlink leaf is never a recursive target, even with a trailing separator");
		assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"foreign");
	}

	#[test]
	fn recursive_cleanup_removes_the_quarantined_directory() {
		let root = tempfile::tempdir().unwrap();
		let owned = root.path().join("owned");
		std::fs::create_dir(&owned).unwrap();
		std::fs::write(owned.join("content"), b"owned").unwrap();

		remove_directory_tree(&owned).unwrap();

		assert!(path_absent(&owned));
		assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
	}
}
