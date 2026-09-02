use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use cap_std::fs::Dir;
use gitana_fs_native::{EntryIdentity, directory_identity, entry_identity};

use crate::SubmoduleError;

/// A locked common-config guard bound to the visible `refs` directory identity.
pub(crate) struct SharedConfigGuard {
	common: Dir,
	refs: Dir,
	refs_entry_identity: EntryIdentity,
	refs_target_identity: EntryIdentity,
	display_path: PathBuf,
	_lock: Option<File>,
}

impl SharedConfigGuard {
	pub(crate) fn new(
		common: Dir,
		refs: Dir,
		refs_entry_identity: EntryIdentity,
		refs_target_identity: EntryIdentity,
		display_path: PathBuf,
		lock: Option<File>,
	) -> Self {
		Self {
			common,
			refs,
			refs_entry_identity,
			refs_target_identity,
			display_path,
			_lock: lock,
		}
	}

	pub(crate) fn validate(&self) -> Result<(), SubmoduleError> {
		if entry_identity(&self.common, OsStr::new("refs")).map_err(|source| {
			changed_or_io(
				&self.display_path,
				source,
				"shared config guard changed while held",
			)
		})? != self.refs_entry_identity
			|| directory_identity(&self.refs).map_err(|source| SubmoduleError::Io {
				path: self.display_path.clone(),
				source,
			})? != self.refs_target_identity
		{
			return Err(SubmoduleError::RecoveryRequired(
				"shared config guard changed while held".to_owned(),
			));
		}
		let visible = self.common.open_dir("refs").map_err(|source| {
			changed_or_io(
				&self.display_path,
				source,
				"shared config guard changed while held",
			)
		})?;
		if directory_identity(&visible).map_err(|source| SubmoduleError::Io {
			path: self.display_path.clone(),
			source,
		})? != self.refs_target_identity
			|| entry_identity(&self.common, OsStr::new("refs")).map_err(|source| {
				changed_or_io(
					&self.display_path,
					source,
					"shared config guard changed while held",
				)
			})? != self.refs_entry_identity
		{
			return Err(SubmoduleError::RecoveryRequired(
				"shared config guard changed while held".to_owned(),
			));
		}
		Ok(())
	}
}

fn changed_or_io(path: &Path, source: std::io::Error, changed: &str) -> SubmoduleError {
	if source.kind() == std::io::ErrorKind::NotFound {
		SubmoduleError::RecoveryRequired(changed.to_owned())
	} else {
		SubmoduleError::Io {
			path: path.to_owned(),
			source,
		}
	}
}
