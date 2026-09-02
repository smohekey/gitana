use cap_std::fs::Dir;
use std::ffi::OsStr;
use std::io::Result;
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
use std::sync::atomic::{AtomicU64, Ordering};

use crate::EntryIdentity;

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
static PRIVATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Rename an entry only when the destination name is still absent.
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
pub fn rename_noreplace(
	source: &Dir,
	source_name: &OsStr,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	use rustix::fs::{RenameFlags, renameat_with};

	renameat_with(
		source,
		source_name,
		destination,
		destination_name,
		RenameFlags::NOREPLACE,
	)
	.map_err(Into::into)
}

/// Rename an entry only when the destination is absent and the published source still has
/// `expected_source` identity.
///
/// Unix has no rename operation conditioned on a source inode. The no-replace rename is therefore
/// followed by an identity check at the destination; a mismatched source is moved back when its old
/// name is still free and is otherwise preserved at the destination while the operation fails.
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
pub fn rename_noreplace_if_identity(
	source: &Dir,
	source_name: &OsStr,
	expected_source: EntryIdentity,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	use crate::entry_identity;
	use rustix::fs::{RenameFlags, renameat_with};
	if entry_identity(source, source_name)? != expected_source {
		return Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"publication source changed before atomic no-replace rename",
		));
	}

	renameat_with(
		source,
		source_name,
		destination,
		destination_name,
		RenameFlags::NOREPLACE,
	)?;
	if entry_identity(destination, destination_name)? == expected_source {
		return Ok(());
	}
	match renameat_with(
		destination,
		destination_name,
		source,
		source_name,
		RenameFlags::NOREPLACE,
	) {
		Ok(()) => Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"publication source changed before atomic no-replace rename",
		)),
		Err(restore) => Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			format!(
				"publication source changed; preserved it at the destination because restoring its name failed: {restore}"
			),
		)),
	}
}

/// Rename an entry only when the destination is absent and the source has `expected_source`
/// identity.
#[cfg(windows)]
pub fn rename_noreplace_if_identity(
	source: &Dir,
	source_name: &OsStr,
	expected_source: EntryIdentity,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	crate::windows::rename_noreplace_if_identity(
		source,
		source_name,
		expected_source,
		destination,
		destination_name,
	)
}

/// Fail closed where source-conditioned no-replace rename is unavailable.
#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn rename_noreplace_if_identity(
	_source: &Dir,
	_source_name: &OsStr,
	_expected_source: EntryIdentity,
	_destination: &Dir,
	_destination_name: &OsStr,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"source-conditioned no-replace rename is unavailable on this platform",
	))
}

/// Rename an entry only when the destination name is still absent.
#[cfg(windows)]
pub fn rename_noreplace(
	source: &Dir,
	source_name: &OsStr,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	crate::windows::rename_noreplace(source, source_name, destination, destination_name)
}

/// Fail closed on native targets without an atomic no-replace primitive.
#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn rename_noreplace(
	_source: &Dir,
	_source_name: &OsStr,
	_destination: &Dir,
	_destination_name: &OsStr,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"atomic no-replace rename is unavailable on this platform",
	))
}

/// Replace `target` with `prepared` only when `target` still has `expected` identity.
///
/// Linux and Apple atomically exchange the two names, inspect the displaced target, and exchange
/// them back on mismatch. Thus a replacement that wins after the caller's final check is preserved
/// rather than overwritten.
#[cfg(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos"
))]
pub fn replace_if_identity(
	directory: &Dir,
	prepared: &OsStr,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	let prepared_identity = crate::entry_identity(directory, prepared)?;
	replace_if_identities(directory, prepared, prepared_identity, target, expected)
}

/// Replace `target` with `prepared` only while both entries retain their captured identities.
///
/// A successful Unix exchange retires the displaced target under a private name. It is deliberately
/// not unlinked afterward: Unix has no delete-by-handle operation, and a path-based unlink would
/// reintroduce a window in which a concurrent replacement could be deleted.
#[cfg(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos"
))]
pub fn replace_if_identities(
	directory: &Dir,
	prepared: &OsStr,
	expected_prepared: EntryIdentity,
	target: &OsStr,
	expected_target: EntryIdentity,
) -> Result<()> {
	use crate::entry_identity;
	use rustix::fs::{RenameFlags, renameat_with};
	if entry_identity(directory, prepared)? != expected_prepared
		|| entry_identity(directory, target)? != expected_target
	{
		return Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"publication source or target changed before atomic replacement",
		));
	}

	renameat_with(
		directory,
		prepared,
		directory,
		target,
		RenameFlags::EXCHANGE,
	)?;
	let published_identity = entry_identity(directory, target)?;
	let displaced_identity = entry_identity(directory, prepared)?;
	if published_identity != expected_prepared || displaced_identity != expected_target {
		let target_is_prepared =
			entry_identity(directory, target).is_ok_and(|identity| identity == published_identity);
		let displaced_is_unchanged =
			entry_identity(directory, prepared).is_ok_and(|identity| identity == displaced_identity);
		if target_is_prepared && displaced_is_unchanged {
			renameat_with(
				directory,
				prepared,
				directory,
				target,
				RenameFlags::EXCHANGE,
			)?;
			return Err(std::io::Error::new(
				std::io::ErrorKind::AlreadyExists,
				"publication source or target changed before atomic replacement",
			));
		}
		return Err(std::io::Error::other(
			"publication source or target changed and the exchanged entries changed before rollback",
		));
	}
	remove_file_if_identity(directory, prepared, expected_target)
}

/// Remove a regular file's active name only while it still has `expected` identity.
///
/// The matched Unix entry is retained under a private quarantine name because a subsequent
/// path-based unlink could delete a concurrent replacement of that private name.
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
pub fn remove_file_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	use crate::entry_identity;
	use rustix::fs::{RenameFlags, renameat_with};

	let quarantine = loop {
		let quarantine = private_absent_name(directory, "remove")?;
		match renameat_with(
			directory,
			target,
			directory,
			&quarantine,
			RenameFlags::NOREPLACE,
		) {
			Ok(()) => break quarantine,
			Err(error)
				if std::io::Error::from_raw_os_error(error.raw_os_error()).kind()
					== std::io::ErrorKind::AlreadyExists
					&& entry_identity(directory, target).is_ok_and(|identity| identity == expected) =>
			{
				continue;
			}
			Err(error) => return Err(error.into()),
		}
	};
	let displaced = entry_identity(directory, &quarantine)?;
	if displaced != expected {
		return match renameat_with(
			directory,
			&quarantine,
			directory,
			target,
			RenameFlags::NOREPLACE,
		) {
			Ok(()) => Err(std::io::Error::new(
				std::io::ErrorKind::AlreadyExists,
				"removal target changed before conditional cleanup",
			)),
			Err(restore) => Err(std::io::Error::new(
				std::io::ErrorKind::AlreadyExists,
				format!(
					"removal target changed; preserved it as '{}' because restoring its name failed: {restore}",
					quarantine.to_string_lossy()
				),
			)),
		};
	}
	// Removing the active name is the strongest portable Unix guarantee. Never resolve the private
	// name again for unlink: a same-user namespace writer could replace it after this validation.
	Ok(())
}

/// Remove a regular file only while its directory entry still has `expected` identity.
#[cfg(windows)]
pub fn remove_file_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	crate::windows::remove_file_if_identity(directory, target, expected)
}

/// Recursively remove a directory only while its entry still has `expected` identity.
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
pub fn remove_dir_all_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	use cap_fs_ext::DirExt as _;

	let opened = directory.open_dir_nofollow(target)?;
	if crate::directory_identity(&opened)? != expected {
		return Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"recursive removal target changed before cleanup",
		));
	}
	opened.remove_open_dir_all()
}

/// Remove an empty directory only while its entry still has `expected` identity.
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
pub fn remove_dir_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	use cap_fs_ext::DirExt as _;

	let opened = directory.open_dir_nofollow(target)?;
	if crate::directory_identity(&opened)? != expected {
		return Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"directory removal target changed before cleanup",
		));
	}
	opened.remove_open_dir()
}

/// Remove an empty directory only while its entry still has `expected` identity.
#[cfg(windows)]
pub fn remove_dir_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	crate::windows::remove_dir_if_identity(directory, target, expected)
}

/// Recursively remove a directory only while its entry still has `expected` identity.
#[cfg(windows)]
pub fn remove_dir_all_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	crate::windows::remove_dir_all_if_identity(directory, target, expected)
}

/// Fail closed where identity-conditioned directory removal is unavailable.
#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn remove_dir_if_identity(
	_directory: &Dir,
	_target: &OsStr,
	_expected: EntryIdentity,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"identity-conditioned directory removal is unavailable on this platform",
	))
}

/// Fail closed where identity-conditioned removal is unavailable.
#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn remove_file_if_identity(
	_directory: &Dir,
	_target: &OsStr,
	_expected: EntryIdentity,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"identity-conditioned file removal is unavailable on this platform",
	))
}

/// Fail closed where identity-conditioned recursive removal is unavailable.
#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "redox",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn remove_dir_all_if_identity(
	_directory: &Dir,
	_target: &OsStr,
	_expected: EntryIdentity,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"identity-conditioned recursive removal is unavailable on this platform",
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
fn private_absent_name(directory: &Dir, purpose: &str) -> Result<std::ffi::OsString> {
	for _ in 0..4096 {
		let sequence = PRIVATE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		let name = format!(
			".gitana-namespace-{purpose}.{}.{}",
			std::process::id(),
			sequence
		);
		match directory.symlink_metadata(&name) {
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name.into()),
			Err(error) => return Err(error),
		}
	}
	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"could not reserve a private namespace name",
	))
}

#[cfg(windows)]
pub fn replace_if_identity(
	directory: &Dir,
	prepared: &OsStr,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	crate::windows::replace_if_identity(directory, prepared, target, expected)
}

/// Replace `target` with `prepared` only while both entries retain their captured identities.
#[cfg(windows)]
pub fn replace_if_identities(
	directory: &Dir,
	prepared: &OsStr,
	expected_prepared: EntryIdentity,
	target: &OsStr,
	expected_target: EntryIdentity,
) -> Result<()> {
	crate::windows::replace_if_identities(
		directory,
		prepared,
		expected_prepared,
		target,
		expected_target,
	)
}

#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn replace_if_identity(
	_directory: &Dir,
	_prepared: &OsStr,
	_target: &OsStr,
	_expected: EntryIdentity,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"identity-conditioned replacement is unavailable on this platform",
	))
}

#[cfg(not(any(
	target_os = "android",
	target_os = "ios",
	target_os = "linux",
	target_os = "macos",
	target_os = "tvos",
	target_os = "visionos",
	target_os = "watchos",
	windows
)))]
pub fn replace_if_identities(
	_directory: &Dir,
	_prepared: &OsStr,
	_expected_prepared: EntryIdentity,
	_target: &OsStr,
	_expected_target: EntryIdentity,
) -> Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"identity-conditioned replacement is unavailable on this platform",
	))
}

#[cfg(test)]
mod tests {
	use super::{
		remove_dir_all_if_identity, remove_file_if_identity, rename_noreplace,
		rename_noreplace_if_identity, replace_if_identities, replace_if_identity,
	};
	use crate::{directory_identity, entry_identity};
	use cap_std::{ambient_authority, fs::Dir};
	use std::ffi::OsStr;

	#[test]
	fn no_replace_preserves_a_raced_file() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("prepared", b"prepared").unwrap();
		directory.write("target", b"foreign").unwrap();

		assert!(
			rename_noreplace(
				&directory,
				OsStr::new("prepared"),
				&directory,
				OsStr::new("target")
			)
			.is_err()
		);
		assert_eq!(directory.read("target").unwrap(), b"foreign");
		assert_eq!(directory.read("prepared").unwrap(), b"prepared");
	}

	#[test]
	fn conditional_no_replace_rejects_a_replaced_source() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("prepared", b"owned").unwrap();
		let expected = entry_identity(&directory, OsStr::new("prepared")).unwrap();
		directory.rename("prepared", &directory, "old").unwrap();
		directory.write("prepared", b"foreign").unwrap();

		assert!(
			rename_noreplace_if_identity(
				&directory,
				OsStr::new("prepared"),
				expected,
				&directory,
				OsStr::new("target")
			)
			.is_err()
		);
		assert_eq!(directory.read("prepared").unwrap(), b"foreign");
		assert!(directory.symlink_metadata("target").is_err());
	}

	#[test]
	fn conditional_replace_rejects_a_replaced_source() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("prepared", b"owned-new").unwrap();
		directory.write("target", b"old").unwrap();
		let expected_prepared = entry_identity(&directory, OsStr::new("prepared")).unwrap();
		let expected_target = entry_identity(&directory, OsStr::new("target")).unwrap();
		directory
			.rename("prepared", &directory, "old-prepared")
			.unwrap();
		directory.write("prepared", b"foreign").unwrap();

		assert!(
			replace_if_identities(
				&directory,
				OsStr::new("prepared"),
				expected_prepared,
				OsStr::new("target"),
				expected_target
			)
			.is_err()
		);
		assert_eq!(directory.read("prepared").unwrap(), b"foreign");
		assert_eq!(directory.read("target").unwrap(), b"old");
	}

	#[test]
	fn conditional_replace_restores_a_same_content_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("target", b"same").unwrap();
		let expected = entry_identity(&directory, OsStr::new("target")).unwrap();
		directory.rename("target", &directory, "old").unwrap();
		directory.write("target", b"same").unwrap();
		let replacement = entry_identity(&directory, OsStr::new("target")).unwrap();
		directory.write("prepared", b"new").unwrap();

		assert!(
			replace_if_identity(
				&directory,
				OsStr::new("prepared"),
				OsStr::new("target"),
				expected
			)
			.is_err()
		);
		assert_eq!(
			entry_identity(&directory, OsStr::new("target")).unwrap(),
			replacement
		);
		assert_eq!(directory.read("target").unwrap(), b"same");
		assert_eq!(directory.read("prepared").unwrap(), b"new");
	}

	#[test]
	fn conditional_remove_preserves_a_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("target", b"owned").unwrap();
		let expected = entry_identity(&directory, OsStr::new("target")).unwrap();
		directory.rename("target", &directory, "old").unwrap();
		directory.write("target", b"foreign").unwrap();

		assert!(remove_file_if_identity(&directory, OsStr::new("target"), expected).is_err());
		assert_eq!(directory.read("target").unwrap(), b"foreign");
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
	#[test]
	fn conditional_remove_retires_the_matched_entry_without_unlinking_it() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.write("target", b"owned").unwrap();
		let expected = entry_identity(&directory, OsStr::new("target")).unwrap();

		remove_file_if_identity(&directory, OsStr::new("target"), expected).unwrap();

		assert!(directory.symlink_metadata("target").is_err());
		let retired = directory
			.entries()
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.find(|name| {
				name
					.to_string_lossy()
					.starts_with(".gitana-namespace-remove.")
			})
			.unwrap();
		assert_eq!(directory.read(retired).unwrap(), b"owned");
	}

	#[test]
	fn conditional_recursive_remove_preserves_a_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		directory.create_dir("target").unwrap();
		let opened = directory.open_dir("target").unwrap();
		let expected = directory_identity(&opened).unwrap();
		directory.rename("target", &directory, "old").unwrap();
		directory.create_dir("target").unwrap();
		directory.write("target/keep", b"foreign").unwrap();

		assert!(remove_dir_all_if_identity(&directory, OsStr::new("target"), expected).is_err());
		assert_eq!(directory.read("target/keep").unwrap(), b"foreign");
	}
}
