//! Audited Windows namespace operations that are not available through Rust's safe filesystem API.
//!
//! Every operation starts from retained directory and entry handles. Names are passed to
//! `SetFileInformationByHandle` relative to the retained destination directory, so an ambient path
//! replacement cannot redirect publication.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, File, OpenOptions, OpenOptionsExt as _};
use std::ffi::{OsStr, OsString};
use std::io::{Error, ErrorKind, Result};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
	DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
	FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
	FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
	FILE_SHARE_WRITE, FileDispositionInfoEx, FileRenameInfoEx, SetFileInformationByHandle,
};

use crate::{EntryIdentity, file_identity};

static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn rename_noreplace(
	source: &Dir,
	source_name: &OsStr,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	let source = open_rename_handle(source, source_name)?;
	rename_handle(&source, destination, destination_name)
}

pub(crate) fn rename_noreplace_if_identity(
	source: &Dir,
	source_name: &OsStr,
	expected_source: EntryIdentity,
	destination: &Dir,
	destination_name: &OsStr,
) -> Result<()> {
	let source = open_rename_handle(source, source_name)?;
	if file_identity(&source)? != expected_source {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"publication source changed before atomic no-replace rename",
		));
	}
	rename_handle(&source, destination, destination_name)
}

pub(crate) fn replace_if_identity(
	directory: &Dir,
	prepared: &OsStr,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	let prepared_file = open_rename_handle(directory, prepared)?;
	let expected_prepared = file_identity(&prepared_file)?;
	replace_open_files(
		directory,
		prepared_file,
		expected_prepared,
		target,
		expected,
	)
}

pub(crate) fn replace_if_identities(
	directory: &Dir,
	prepared: &OsStr,
	expected_prepared: EntryIdentity,
	target: &OsStr,
	expected_target: EntryIdentity,
) -> Result<()> {
	let prepared_file = open_rename_handle(directory, prepared)?;
	replace_open_files(
		directory,
		prepared_file,
		expected_prepared,
		target,
		expected_target,
	)
}

fn replace_open_files(
	directory: &Dir,
	prepared_file: File,
	expected_prepared: EntryIdentity,
	target: &OsStr,
	expected_target: EntryIdentity,
) -> Result<()> {
	if file_identity(&prepared_file)? != expected_prepared {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"publication source changed before atomic replacement",
		));
	}
	let target_file = open_rename_handle(directory, target)?;
	if file_identity(&target_file)? != expected_target {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"publication target changed before atomic replacement",
		));
	}

	let backup = private_backup_name();
	rename_handle(&target_file, directory, &backup)?;
	if let Err(publication) = rename_handle(&prepared_file, directory, target) {
		return match rename_handle(&target_file, directory, target) {
			Ok(()) => Err(publication),
			Err(rollback) => Err(Error::new(
				publication.kind(),
				format!(
					"{publication}; preserving the displaced target as '{}' because rollback failed: {rollback}",
					backup.to_string_lossy()
				),
			)),
		};
	}
	delete_handle(&target_file)
}

pub(crate) fn remove_file_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	let target = open_delete_handle(directory, target, false)?;
	if file_identity(&target)? != expected {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"removal target changed before conditional cleanup",
		));
	}
	delete_handle(&target)
}

pub(crate) fn remove_dir_all_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	let target = open_delete_handle(directory, target, true)?;
	if file_identity(&target)? != expected {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"recursive removal target changed before cleanup",
		));
	}
	let contents = Dir::from_std_file(target.try_clone()?.into_std());
	remove_directory_contents(&contents)?;
	drop(contents);
	delete_handle(&target)
}

pub(crate) fn remove_dir_if_identity(
	directory: &Dir,
	target: &OsStr,
	expected: EntryIdentity,
) -> Result<()> {
	let target = open_delete_handle(directory, target, true)?;
	if file_identity(&target)? != expected {
		return Err(Error::new(
			ErrorKind::AlreadyExists,
			"directory removal target changed before cleanup",
		));
	}
	delete_handle(&target)
}

pub(crate) fn os_str_eq_ignore_case(left: &OsStr, right: &OsStr) -> bool {
	let left: Vec<u16> = left.encode_wide().collect();
	let right: Vec<u16> = right.encode_wide().collect();
	let Ok(left_len) = i32::try_from(left.len()) else {
		return false;
	};
	let Ok(right_len) = i32::try_from(right.len()) else {
		return false;
	};
	// SAFETY: the slices remain alive for the call and their lengths were checked to fit the API.
	unsafe {
		CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
	}
}

fn remove_directory_contents(directory: &Dir) -> Result<()> {
	let names = directory
		.entries()?
		.map(|entry| entry.map(|entry| entry.file_name()))
		.collect::<Result<Vec<_>>>()?;
	for name in names {
		let child = open_delete_handle(directory, &name, true)?;
		let metadata = child.metadata()?;
		if metadata.is_dir() && !metadata.file_type().is_symlink() {
			let child_directory = Dir::from_std_file(child.try_clone()?.into_std());
			remove_directory_contents(&child_directory)?;
		}
		delete_handle(&child)?;
	}
	Ok(())
}

fn open_rename_handle(directory: &Dir, name: &OsStr) -> Result<File> {
	open_delete_handle(directory, name, false)
}

fn open_delete_handle(directory: &Dir, name: &OsStr, list_directory: bool) -> Result<File> {
	let mut options = OpenOptions::new();
	options
		.access_mode(
			DELETE
				| FILE_READ_ATTRIBUTES
				| if list_directory {
					FILE_LIST_DIRECTORY
				} else {
					0
				},
		)
		.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
		.follow(FollowSymlinks::No);
	directory.open_with(name, &options)
}

fn rename_handle(source: &File, destination: &Dir, destination_name: &OsStr) -> Result<()> {
	let name: Vec<u16> = destination_name.encode_wide().collect();
	let name_bytes = name
		.len()
		.checked_mul(std::mem::size_of::<u16>())
		.and_then(|bytes| u32::try_from(bytes).ok())
		.ok_or_else(|| {
			Error::new(
				ErrorKind::InvalidInput,
				"Windows destination name is too long",
			)
		})?;
	let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
	let bytes = header.checked_add(name_bytes as usize).ok_or_else(|| {
		Error::new(
			ErrorKind::InvalidInput,
			"Windows destination name is too long",
		)
	})?;
	let words = bytes.div_ceil(std::mem::size_of::<usize>());
	let mut storage = vec![0usize; words];
	let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
	// SAFETY: `storage` is word-aligned and sized for the fixed header plus exactly `name_bytes`.
	// `destination` and `source` stay alive for the call, and the API copies the supplied name.
	unsafe {
		(*information).Anonymous.Flags = 0;
		(*information).RootDirectory = destination.as_raw_handle() as HANDLE;
		(*information).FileNameLength = name_bytes;
		std::ptr::copy_nonoverlapping(
			name.as_ptr(),
			(*information).FileName.as_mut_ptr(),
			name.len(),
		);
		if SetFileInformationByHandle(
			source.as_raw_handle() as HANDLE,
			FileRenameInfoEx,
			information.cast(),
			bytes as u32,
		) == 0
		{
			return Err(Error::last_os_error());
		}
	}
	Ok(())
}

fn delete_handle(file: &File) -> Result<()> {
	let information = FILE_DISPOSITION_INFO_EX {
		Flags: FILE_DISPOSITION_FLAG_DELETE
			| FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
			| FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
	};
	// SAFETY: `information` is the exact structure required by `FileDispositionInfoEx`; `file`
	// remains open for the call, so deletion is tied to the captured entry rather than a path name.
	unsafe {
		if SetFileInformationByHandle(
			file.as_raw_handle() as HANDLE,
			FileDispositionInfoEx,
			std::ptr::from_ref(&information).cast(),
			std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
		) == 0
		{
			return Err(Error::last_os_error());
		}
	}
	Ok(())
}

fn private_backup_name() -> OsString {
	let sequence = BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
	format!(
		".gitana-namespace-backup.{}.{}",
		std::process::id(),
		sequence
	)
	.into()
}
