//! Deriving git blob ids and modes from a working-tree capability's metadata.
//!
//! Everything here reads from a [`Meta`] (the capability's `lstat` result) rather than a
//! `std::fs::Metadata`, so a native (cap-std) capability keeps full fidelity — real exec bit, real
//! `stat(2)` identity for the index cache — while a WASI capability degrades where `descriptor-stat`
//! is silent (exec bit collapses to `100644`; the zeroed stat identity forces a re-hash). Behaviour
//! follows the capability, not a compile-time `cfg(unix)` split.

use gitana_file_store_local::{Meta, WorkDirFs};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};

use crate::ignore::{self, DirIgnore};
use crate::{Stat, WorktreeError};

/// Join a work-tree-relative directory and an entry name into a `/`-separated path (an empty
/// `dir_rel` — the work-tree root — yields the bare name).
pub(crate) fn join_rel(dir_rel: &str, name: &str) -> String {
	if dir_rel.is_empty() {
		name.to_owned()
	} else {
		format!("{dir_rel}/{name}")
	}
}

/// Read `dir_rel`'s `.gitignore` (if any) through `work`, parse it relative to `dir_rel`, and push
/// it onto `stack`; returns whether one was present (so the caller can pop it after descending).
pub(crate) fn push_gitignore<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
) -> Result<bool, WorktreeError> {
	match work.read(&join_rel(dir_rel, ".gitignore")) {
		Ok(bytes) => {
			stack.push(ignore::parse(&String::from_utf8_lossy(&bytes), dir_rel));
			Ok(true)
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(error) => Err(error.into()),
	}
}

/// Hash a working-tree entry into a blob id (without writing it) with its git mode, reading its
/// content (or symlink target) through `work`. `None` for anything that is neither a regular file
/// nor a symlink.
pub(crate) fn blob_of<W: WorkDirFs, H: HashAlgorithm>(
	work: &W,
	path: &str,
	meta: &Meta,
) -> std::io::Result<Option<(ObjectId<H>, u32)>> {
	if meta.kind.is_symlink() {
		let target = work.read_link(path)?;
		Ok(Some((
			ObjectId::<H>::compute(ObjectKind::Blob, &target),
			0o120000,
		)))
	} else if meta.kind.is_file() {
		let content = work.read(path)?;
		Ok(Some((
			ObjectId::<H>::compute(ObjectKind::Blob, &content),
			file_mode(meta),
		)))
	} else {
		Ok(None)
	}
}

/// The git mode for a regular file (`100755` if any execute bit is set). A capability that cannot
/// report the mode (WASI) leaves it `0`, which reads as `100644` — git's `core.fileMode=false`.
pub(crate) fn file_mode(meta: &Meta) -> u32 {
	if meta.mode & 0o111 != 0 {
		0o100755
	} else {
		0o100644
	}
}

/// The git mode for an lstat'ed entry (symlink, executable, or regular file).
pub(crate) fn mode_of(meta: &Meta) -> u32 {
	if meta.kind.is_symlink() {
		0o120000
	} else {
		file_mode(meta)
	}
}

/// The index stat cache for a working-tree file. A capability that cannot report a field (WASI)
/// leaves it `0`; the resulting cache never matches a re-`lstat`, so `status`/`diff` re-hash — git's
/// `core.checkStat=minimal`.
pub(crate) fn stat_of(meta: &Meta) -> Stat {
	Stat {
		ctime_sec: meta.ctime.0 as u32,
		ctime_nsec: meta.ctime.1,
		mtime_sec: meta.mtime.0 as u32,
		mtime_nsec: meta.mtime.1,
		dev: meta.dev as u32,
		ino: meta.ino as u32,
		uid: meta.uid,
		gid: meta.gid,
		size: meta.size as u32,
	}
}
