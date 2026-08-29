use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store_local::same_directory_identity;
use gitana_fs_native::{
	EntryIdentity, directory_identity, entry_identity, remove_dir_if_identity, rename_noreplace,
};

static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_PRIVATE_NAMES: u32 = 4096;

/// Attempt-owned clone destination state.
///
/// Clone materialization happens in a private sibling directory. The requested destination remains
/// an empty identity-pinned reservation until the completed clone is published, so failure cleanup
/// never enumerates or recursively removes user-owned destination contents.
pub(crate) struct CloneDestination {
	target: PathBuf,
	preexisting: bool,
	attempt: Option<CloneAttempt>,
}

struct CloneAttempt {
	parent: Dir,
	parent_path: PathBuf,
	target_name: OsString,
	reservation: Option<Dir>,
	reservation_owned: bool,
	staging_name: OsString,
	staging: Option<Dir>,
}

impl CloneDestination {
	pub(crate) fn new(path: &Path, preexisting: bool) -> Self {
		Self {
			target: path.to_owned(),
			preexisting,
			attempt: None,
		}
	}

	/// Reserve the requested destination and return the private worktree path for this clone.
	pub(crate) fn start(&mut self) -> std::io::Result<PathBuf> {
		if let Some(attempt) = &self.attempt {
			return Ok(attempt.parent_path.join(&attempt.staging_name));
		}

		let absolute = std::path::absolute(&self.target)?;
		let target_name = absolute
			.file_name()
			.ok_or_else(|| invalid_input("clone destination has no final path component"))?
			.to_owned();
		let requested_parent = absolute
			.parent()
			.ok_or_else(|| invalid_input("clone destination has no parent directory"))?;
		std::fs::create_dir_all(requested_parent)?;
		let parent_path = std::fs::canonicalize(requested_parent)?;
		let parent = Dir::open_ambient_dir(&parent_path, ambient_authority())?;

		let (reservation, reservation_owned) = if self.preexisting {
			let reservation = parent.open_dir_nofollow(&target_name)?;
			if !directory_is_empty(&reservation)? {
				return Err(std::io::Error::new(
					std::io::ErrorKind::DirectoryNotEmpty,
					"clone destination gained content before cloning started",
				));
			}
			(reservation, false)
		} else {
			parent.create_dir(&target_name)?;
			(parent.open_dir_nofollow(&target_name)?, true)
		};

		let staging_name = create_private_directory(&parent, "stage")?;
		let staging = match parent.open_dir_nofollow(&staging_name) {
			Ok(staging) => staging,
			Err(error) => {
				let _ = parent.remove_dir(&staging_name);
				if reservation_owned {
					let _ = reservation.remove_open_dir();
				}
				return Err(error);
			}
		};
		let staging_path = parent_path.join(&staging_name);
		self.attempt = Some(CloneAttempt {
			parent,
			parent_path,
			target_name,
			reservation: Some(reservation),
			reservation_owned,
			staging_name,
			staging: Some(staging),
		});
		Ok(staging_path)
	}

	/// Publish a completed staged clone in place of the unchanged empty reservation.
	pub(crate) fn commit(mut self) -> std::io::Result<()> {
		let Some(attempt) = self.attempt.as_mut() else {
			return Ok(());
		};
		publish(attempt)?;
		self.attempt = None;
		Ok(())
	}

	pub(crate) fn cleanup_after_failure(&mut self) -> std::io::Result<()> {
		let Some(attempt) = self.attempt.as_mut() else {
			return Ok(());
		};
		cleanup(attempt)?;
		self.attempt = None;
		Ok(())
	}
}

impl Drop for CloneDestination {
	fn drop(&mut self) {
		if let Some(attempt) = self.attempt.as_mut() {
			let _ = cleanup(attempt);
		}
	}
}

fn publish(attempt: &mut CloneAttempt) -> std::io::Result<()> {
	let reservation = attempt
		.reservation
		.as_ref()
		.ok_or_else(|| invalid_data("clone reservation is no longer available"))?;
	let current = attempt.parent.open_dir_nofollow(&attempt.target_name)?;
	if !same_directory_identity(reservation, &current)? {
		return Err(invalid_data(
			"clone destination identity changed before publication",
		));
	}
	if !directory_is_empty(&current)? {
		return Err(std::io::Error::new(
			std::io::ErrorKind::DirectoryNotEmpty,
			"clone destination gained content before publication",
		));
	}
	drop(current);
	if !attempt.reservation_owned {
		return publish_into_preexisting(attempt);
	}
	publish_replacing_owned_reservation(attempt)
}

fn publish_into_preexisting(attempt: &mut CloneAttempt) -> std::io::Result<()> {
	publish_into_preexisting_with(attempt, |_, _| Ok(()))
}

fn publish_into_preexisting_with(
	attempt: &mut CloneAttempt,
	mut after_publish: impl FnMut(&Dir, &PublishedEntry) -> std::io::Result<()>,
) -> std::io::Result<()> {
	let reservation = attempt
		.reservation
		.as_ref()
		.ok_or_else(|| invalid_data("clone reservation is no longer available"))?;
	let staging = attempt
		.staging
		.as_ref()
		.ok_or_else(|| invalid_data("clone staging directory is no longer available"))?;
	let mut names: Vec<OsString> = staging
		.entries()?
		.map(|entry| entry.map(|entry| entry.file_name()))
		.collect::<std::io::Result<_>>()?;
	names.sort();
	let mut moved = Vec::with_capacity(names.len());
	for name in names {
		let identity = entry_identity(staging, &name)?;
		if let Err(error) = publish_clone_entry(staging, reservation, &name) {
			return Err(publication_failure(error, &moved));
		}
		moved.push(PublishedEntry { name, identity });
		let published = moved.last().expect("just published one entry");
		match entry_identity(reservation, &published.name) {
			Ok(actual) if actual == published.identity => {}
			Ok(_) => {
				return Err(publication_failure(
					invalid_data("clone entry identity changed during publication"),
					&moved,
				));
			}
			Err(error) => {
				return Err(publication_failure(error, &moved));
			}
		}
		if let Err(error) = after_publish(reservation, published) {
			return Err(publication_failure(error, &moved));
		}
	}

	let current = match attempt.parent.open_dir_nofollow(&attempt.target_name) {
		Ok(current) => current,
		Err(error) => return Err(publication_failure(error, &moved)),
	};
	let unchanged = match same_directory_identity(reservation, &current) {
		Ok(unchanged) => unchanged,
		Err(error) => return Err(publication_failure(error, &moved)),
	};
	drop(current);
	if !unchanged {
		return Err(publication_failure(
			invalid_data("clone destination identity changed during publication"),
			&moved,
		));
	}
	if let Err(error) = validate_published_entries(reservation, &moved) {
		return Err(publication_failure(error, &moved));
	}
	let staging_for_removal = match staging.try_clone() {
		Ok(staging) => staging,
		Err(error) => return Err(publication_failure(error, &moved)),
	};
	if let Err(error) = staging_for_removal.remove_open_dir() {
		return Err(publication_failure(error, &moved));
	}
	attempt.staging.take();
	attempt.reservation.take();
	Ok(())
}

fn publish_clone_entry(staging: &Dir, destination: &Dir, name: &OsStr) -> std::io::Result<()> {
	rename_noreplace(staging, name, destination, name)
}

fn publish_replacing_owned_reservation(attempt: &mut CloneAttempt) -> std::io::Result<()> {
	publish_replacing_owned_reservation_with(attempt, same_directory_identity)
}

fn publish_replacing_owned_reservation_with(
	attempt: &mut CloneAttempt,
	mut identity_matches: impl FnMut(&Dir, &Dir) -> std::io::Result<bool>,
) -> std::io::Result<()> {
	let reservation = attempt
		.reservation
		.as_ref()
		.ok_or_else(|| invalid_data("clone reservation is no longer available"))?;

	let backup_name = private_absent_name(&attempt.parent, "reservation")?;
	rename_noreplace(
		&attempt.parent,
		&attempt.target_name,
		&attempt.parent,
		&backup_name,
	)?;
	let backup = match attempt.parent.open_dir_nofollow(&backup_name) {
		Ok(backup) => backup,
		Err(error) => {
			let _ = restore_or_discard_owned_reservation(attempt, &backup_name);
			return Err(error);
		}
	};
	let unchanged = match identity_matches(reservation, &backup) {
		Ok(unchanged) => unchanged,
		Err(error) => {
			drop(backup);
			let _ = restore_or_discard_owned_reservation(attempt, &backup_name);
			return Err(error);
		}
	};
	if !unchanged {
		drop(backup);
		let _ = restore_or_discard_owned_reservation(attempt, &backup_name);
		return Err(invalid_data(
			"clone destination identity changed while it was reserved",
		));
	}

	if let Err(error) = rename_noreplace(
		&attempt.parent,
		&attempt.staging_name,
		&attempt.parent,
		&attempt.target_name,
	) {
		drop(backup);
		let _ = restore_or_discard_owned_reservation(attempt, &backup_name);
		return Err(error);
	}
	let published = match attempt.parent.open_dir_nofollow(&attempt.target_name) {
		Ok(published) => published,
		Err(error) => {
			rollback_publication(attempt, &backup_name);
			return Err(error);
		}
	};
	let staging = match attempt.staging.as_ref() {
		Some(staging) => staging,
		None => {
			drop(published);
			rollback_publication(attempt, &backup_name);
			return Err(invalid_data(
				"clone staging directory is no longer available",
			));
		}
	};
	let unchanged = match identity_matches(staging, &published) {
		Ok(unchanged) => unchanged,
		Err(error) => {
			drop(published);
			rollback_publication(attempt, &backup_name);
			return Err(error);
		}
	};
	if !unchanged {
		drop(published);
		rollback_publication(attempt, &backup_name);
		return Err(invalid_data(
			"clone staging identity changed during publication",
		));
	}
	drop(published);

	// `remove_open_dir` is deliberately non-recursive: content concurrently added through a retained
	// handle makes publication fail and is restored at the requested path instead of being deleted.
	attempt.reservation.take();
	if let Err(error) = backup.remove_open_dir() {
		rollback_publication(attempt, &backup_name);
		return Err(error);
	}
	attempt.staging.take();
	Ok(())
}

#[derive(Debug)]
struct PublishedEntry {
	name: OsString,
	identity: EntryIdentity,
}

fn publication_failure(error: std::io::Error, moved: &[PublishedEntry]) -> std::io::Error {
	if moved.is_empty() {
		error
	} else {
		// Once an entry is visible in a caller-owned directory, another process may open it or add
		// nested content without changing the top-level inode. Moving that entry back into private
		// staging would make Drop recursively delete data that this attempt does not own.
		std::io::Error::new(
			error.kind(),
			format!(
				"{error}; retained {} already-published clone entr{} to preserve concurrent destination content",
				moved.len(),
				if moved.len() == 1 { "y" } else { "ies" }
			),
		)
	}
}

fn validate_published_entries(
	directory: &Dir,
	published: &[PublishedEntry],
) -> std::io::Result<()> {
	let mut actual: Vec<OsString> = directory
		.entries()?
		.map(|entry| entry.map(|entry| entry.file_name()))
		.collect::<std::io::Result<_>>()?;
	actual.sort();
	let mut expected: Vec<&OsStr> = published
		.iter()
		.map(|entry| entry.name.as_os_str())
		.collect();
	expected.sort();
	if !actual.iter().map(OsString::as_os_str).eq(expected) {
		return Err(std::io::Error::new(
			std::io::ErrorKind::DirectoryNotEmpty,
			"clone destination gained or lost content during publication",
		));
	}
	for entry in published {
		if entry_identity(directory, &entry.name)? != entry.identity {
			return Err(invalid_data(
				"clone entry identity changed before publication commit",
			));
		}
	}
	Ok(())
}

fn rollback_publication(attempt: &mut CloneAttempt, backup_name: &OsStr) {
	if let (Some(staging), Ok(current)) = (
		attempt.staging.as_ref(),
		attempt.parent.open_dir_nofollow(&attempt.target_name),
	) && same_directory_identity(staging, &current).unwrap_or(false)
	{
		let _ = rename_noreplace(
			&attempt.parent,
			&attempt.target_name,
			&attempt.parent,
			&attempt.staging_name,
		);
	}
	let _ = restore_or_discard_owned_reservation(attempt, backup_name);
}

fn restore_or_discard_owned_reservation(
	attempt: &mut CloneAttempt,
	backup_name: &OsStr,
) -> std::io::Result<()> {
	match rename_noreplace(
		&attempt.parent,
		backup_name,
		&attempt.parent,
		&attempt.target_name,
	) {
		Ok(()) => Ok(()),
		Err(_) => {
			// A concurrently created destination wins. Remove only the unchanged empty
			// reservation retained by this attempt, never the raced destination.
			let reservation = attempt
				.reservation
				.as_ref()
				.ok_or_else(|| invalid_data("clone reservation is no longer available"))?;
			let identity = directory_identity(reservation)?;
			attempt.reservation.take();
			remove_dir_if_identity(&attempt.parent, backup_name, identity)
		}
	}
}

fn cleanup(attempt: &mut CloneAttempt) -> std::io::Result<()> {
	let staging_result = remove_owned_directory(
		&attempt.parent,
		&attempt.staging_name,
		&mut attempt.staging,
		true,
	);
	let reservation_result = if attempt.reservation_owned {
		remove_owned_directory(
			&attempt.parent,
			&attempt.target_name,
			&mut attempt.reservation,
			false,
		)
	} else {
		Ok(())
	};
	staging_result.and(reservation_result)
}

fn remove_owned_directory(
	parent: &Dir,
	name: &OsStr,
	owned: &mut Option<Dir>,
	recursive: bool,
) -> std::io::Result<()> {
	let Some(original) = owned.take() else {
		return Ok(());
	};
	let current = match parent.open_dir_nofollow(name) {
		Ok(current) => current,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error),
	};
	if !same_directory_identity(&original, &current)? {
		return Err(invalid_data(
			"attempt-owned clone directory was replaced before cleanup",
		));
	}
	drop(current);

	let trash_name = private_absent_name(parent, "cleanup")?;
	rename_noreplace(parent, name, parent, &trash_name)?;
	let moved = match parent.open_dir_nofollow(&trash_name) {
		Ok(moved) => moved,
		Err(error) => {
			restore_rename(parent, &trash_name, name);
			return Err(error);
		}
	};
	if !same_directory_identity(&original, &moved)? {
		drop(moved);
		restore_rename(parent, &trash_name, name);
		return Err(invalid_data(
			"attempt-owned clone directory changed identity during cleanup",
		));
	}
	drop(original);
	let result = if recursive {
		moved.remove_open_dir_all()
	} else {
		moved.remove_open_dir()
	};
	if let Err(error) = result {
		restore_rename(parent, &trash_name, name);
		return Err(error);
	}
	Ok(())
}

fn create_private_directory(parent: &Dir, purpose: &str) -> std::io::Result<OsString> {
	for _ in 0..MAX_PRIVATE_NAMES {
		let name = private_name(purpose);
		match parent.create_dir(&name) {
			Ok(()) => return Ok(name),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
			Err(error) => return Err(error),
		}
	}
	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"exhausted private clone directory names",
	))
}

fn private_absent_name(parent: &Dir, purpose: &str) -> std::io::Result<OsString> {
	for _ in 0..MAX_PRIVATE_NAMES {
		let name = private_name(purpose);
		match parent.symlink_metadata(&name) {
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
			Err(error) => return Err(error),
		}
	}
	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"exhausted private clone directory names",
	))
}

fn private_name(purpose: &str) -> OsString {
	let sequence = ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
	format!(
		".gitana-clone.{purpose}.{}.{}",
		std::process::id(),
		sequence
	)
	.into()
}

fn restore_rename(parent: &Dir, from: &OsStr, to: &OsStr) {
	let _ = rename_noreplace(parent, from, parent, to);
}

fn directory_is_empty(directory: &Dir) -> std::io::Result<bool> {
	Ok(directory.entries()?.next().transpose()?.is_none())
}

fn invalid_input(message: &'static str) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clone_entry_publication_preserves_a_raced_file() {
		let temporary = tempfile::tempdir().unwrap();
		let staging_path = temporary.path().join("staging");
		let destination_path = temporary.path().join("destination");
		std::fs::create_dir(&staging_path).unwrap();
		std::fs::create_dir(&destination_path).unwrap();
		std::fs::write(staging_path.join("entry"), b"clone").unwrap();
		std::fs::write(destination_path.join("entry"), b"foreign").unwrap();
		let staging = Dir::open_ambient_dir(&staging_path, ambient_authority()).unwrap();
		let destination = Dir::open_ambient_dir(&destination_path, ambient_authority()).unwrap();

		assert!(publish_clone_entry(&staging, &destination, OsStr::new("entry")).is_err());
		assert_eq!(
			std::fs::read(destination_path.join("entry")).unwrap(),
			b"foreign"
		);
		assert_eq!(std::fs::read(staging_path.join("entry")).unwrap(), b"clone");
	}

	#[test]
	fn clone_entry_publication_preserves_a_raced_empty_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let staging_path = temporary.path().join("staging");
		let destination_path = temporary.path().join("destination");
		std::fs::create_dir(&staging_path).unwrap();
		std::fs::create_dir(&destination_path).unwrap();
		std::fs::create_dir(staging_path.join("entry")).unwrap();
		std::fs::create_dir(destination_path.join("entry")).unwrap();
		let staged = Dir::open_ambient_dir(staging_path.join("entry"), ambient_authority()).unwrap();
		let raced = Dir::open_ambient_dir(destination_path.join("entry"), ambient_authority()).unwrap();
		let staging = Dir::open_ambient_dir(&staging_path, ambient_authority()).unwrap();
		let destination = Dir::open_ambient_dir(&destination_path, ambient_authority()).unwrap();

		assert!(publish_clone_entry(&staging, &destination, OsStr::new("entry")).is_err());
		let staged_after =
			Dir::open_ambient_dir(staging_path.join("entry"), ambient_authority()).unwrap();
		let raced_after =
			Dir::open_ambient_dir(destination_path.join("entry"), ambient_authority()).unwrap();
		assert!(same_directory_identity(&staged, &staged_after).unwrap());
		assert!(same_directory_identity(&raced, &raced_after).unwrap());
	}

	#[test]
	fn absent_clone_publication_preserves_a_raced_destination() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let mut destination = CloneDestination::new(&target, false);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"clone").unwrap();
		let mut injected = false;

		let result = publish_replacing_owned_reservation_with(
			destination.attempt.as_mut().unwrap(),
			|left, right| {
				if !injected {
					std::fs::create_dir(&target)?;
					injected = true;
				}
				same_directory_identity(left, right)
			},
		);

		assert!(result.is_err());
		assert!(std::fs::read_dir(&target).unwrap().next().is_none());
		assert!(!has_private_reservation(temporary.path()));
	}

	#[test]
	fn failed_clone_preserves_concurrent_content_in_a_preexisting_destination() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		std::fs::create_dir(&target).unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"attempt").unwrap();
		std::fs::write(target.join("keep"), b"concurrent").unwrap();

		destination.cleanup_after_failure().unwrap();
		assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"concurrent");
		assert!(!staging.exists());
	}

	#[cfg(unix)]
	#[test]
	fn failed_clone_does_not_follow_a_replacement_destination_symlink() {
		use std::os::unix::fs::symlink;

		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let original = temporary.path().join("original");
		let foreign = temporary.path().join("foreign");
		std::fs::create_dir(&target).unwrap();
		std::fs::create_dir(&foreign).unwrap();
		std::fs::write(foreign.join("keep"), b"foreign").unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"attempt").unwrap();
		std::fs::rename(&target, &original).unwrap();
		symlink(&foreign, &target).unwrap();

		destination.cleanup_after_failure().unwrap();
		assert!(
			std::fs::symlink_metadata(&target)
				.unwrap()
				.file_type()
				.is_symlink()
		);
		assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"foreign");
		assert!(!staging.exists());
	}

	#[test]
	fn failed_clone_removes_only_its_unchanged_absent_destination_reservation() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let mut destination = CloneDestination::new(&target, false);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"attempt").unwrap();

		destination.cleanup_after_failure().unwrap();
		assert!(!target.exists());
		assert!(!staging.exists());
	}

	#[test]
	fn reservation_identity_error_restores_before_failed_clone_cleanup() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let mut destination = CloneDestination::new(&target, false);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"attempt").unwrap();

		let error =
			publish_replacing_owned_reservation_with(destination.attempt.as_mut().unwrap(), |_, _| {
				Err(std::io::Error::other("injected identity metadata failure"))
			})
			.unwrap_err();
		assert!(
			error
				.to_string()
				.contains("injected identity metadata failure")
		);
		assert!(target.is_dir(), "the reservation must be restored");
		assert!(staging.join("artifact").is_file());
		assert!(!has_private_reservation(temporary.path()));

		destination.cleanup_after_failure().unwrap();
		assert!(!target.exists());
		assert!(!staging.exists());
		assert!(!has_private_reservation(temporary.path()));
	}

	#[test]
	fn published_identity_error_rolls_back_before_failed_clone_cleanup() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let mut destination = CloneDestination::new(&target, false);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("artifact"), b"attempt").unwrap();
		let mut identity_checks = 0;

		let error = publish_replacing_owned_reservation_with(
			destination.attempt.as_mut().unwrap(),
			|left, right| {
				identity_checks += 1;
				if identity_checks == 1 {
					same_directory_identity(left, right)
				} else {
					Err(std::io::Error::other("injected published metadata failure"))
				}
			},
		)
		.unwrap_err();
		assert!(
			error
				.to_string()
				.contains("injected published metadata failure")
		);
		assert!(target.is_dir(), "the reservation must be restored");
		assert!(
			staging.join("artifact").is_file(),
			"the clone must return to staging"
		);
		assert!(!has_private_reservation(temporary.path()));

		destination.cleanup_after_failure().unwrap();
		assert!(!target.exists());
		assert!(!staging.exists());
		assert!(!has_private_reservation(temporary.path()));
	}

	#[test]
	fn completed_clone_populates_the_original_empty_reservation() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		std::fs::create_dir(&target).unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
		}
		let original = Dir::open_ambient_dir(&target, ambient_authority()).unwrap();
		let before = std::fs::metadata(&target).unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("published"), b"clone").unwrap();

		destination.commit().unwrap();
		let published = Dir::open_ambient_dir(&target, ambient_authority()).unwrap();
		assert!(same_directory_identity(&original, &published).unwrap());
		assert_eq!(std::fs::read(target.join("published")).unwrap(), b"clone");
		assert!(!staging.exists());
		#[cfg(unix)]
		{
			use std::os::unix::fs::{MetadataExt, PermissionsExt};
			let after = std::fs::metadata(&target).unwrap();
			assert_eq!(after.dev(), before.dev());
			assert_eq!(after.ino(), before.ino());
			assert_eq!(after.uid(), before.uid());
			assert_eq!(after.gid(), before.gid());
			assert_eq!(after.permissions().mode() & 0o777, 0o700);
		}
	}

	#[test]
	fn publication_preserves_content_added_to_a_preexisting_destination() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		std::fs::create_dir(&target).unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("published"), b"clone").unwrap();
		std::fs::write(target.join("keep"), b"concurrent").unwrap();

		assert!(destination.commit().is_err());
		assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"concurrent");
		assert!(!target.join("published").exists());
		assert!(!staging.exists());
	}

	#[test]
	fn failed_publication_never_moves_nested_concurrent_writes_into_cleanup_staging() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		std::fs::create_dir(&target).unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::create_dir(staging.join("published")).unwrap();
		std::fs::write(staging.join("published/clone"), b"clone").unwrap();
		std::fs::write(staging.join("unpublished"), b"private").unwrap();

		let error =
			publish_into_preexisting_with(destination.attempt.as_mut().unwrap(), |_, published| {
				if published.name == "published" {
					std::fs::write(target.join("published/user"), b"concurrent")?;
					return Err(std::io::Error::other("forced later publication failure"));
				}
				Ok(())
			})
			.unwrap_err();
		assert!(
			error
				.to_string()
				.contains("retained 1 already-published clone entry")
		);

		destination.cleanup_after_failure().unwrap();
		assert_eq!(
			std::fs::read(target.join("published/clone")).unwrap(),
			b"clone"
		);
		assert_eq!(
			std::fs::read(target.join("published/user")).unwrap(),
			b"concurrent"
		);
		assert!(!target.join("unpublished").exists());
		assert!(!staging.exists());
	}

	#[test]
	fn final_publication_rejects_a_replaced_earlier_entry() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let displaced = temporary.path().join("displaced-clone-entry");
		std::fs::create_dir(&target).unwrap();
		let mut destination = CloneDestination::new(&target, true);
		let staging = destination.start().unwrap();
		std::fs::write(staging.join("a"), b"clone a").unwrap();
		std::fs::write(staging.join("b"), b"clone b").unwrap();

		let error =
			publish_into_preexisting_with(destination.attempt.as_mut().unwrap(), |_, published| {
				if published.name == "b" {
					std::fs::rename(target.join("a"), &displaced)?;
					std::fs::write(target.join("a"), b"foreign a")?;
				}
				Ok(())
			})
			.unwrap_err();
		assert!(
			error
				.to_string()
				.contains("clone entry identity changed before publication commit"),
			"unexpected error: {error}"
		);

		destination.cleanup_after_failure().unwrap();
		assert_eq!(std::fs::read(target.join("a")).unwrap(), b"foreign a");
		assert_eq!(std::fs::read(&displaced).unwrap(), b"clone a");
		assert_eq!(std::fs::read(target.join("b")).unwrap(), b"clone b");
		assert!(!staging.exists());
	}

	fn has_private_reservation(parent: &Path) -> bool {
		std::fs::read_dir(parent).unwrap().any(|entry| {
			entry
				.unwrap()
				.file_name()
				.to_string_lossy()
				.starts_with(".gitana-clone.reservation.")
		})
	}
}
