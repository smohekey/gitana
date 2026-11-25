//! The `locked` marker a create holds in a new worktree's admin folder while it builds it, as
//! `git worktree add` does.
//!
//! A half-built admin folder looks stale to git's `should_prune_worktree` (no `gitdir` yet, or a `gitdir`
//! whose checkout `.git` does not exist yet), so any concurrent `git worktree prune` would delete it out
//! from under the create. That prune is not only a user's or an IDE's: git 2.55's automatic maintenance
//! (`git commit` → `git maintenance run --auto --detach`) runs a `worktree-prune` task by default. git
//! protects its own adds by writing `<admin>/locked` (`initializing`) right after creating the folder and
//! removing it once the worktree is complete (`builtin/worktree.c`, `add_worktree`); a locked entry is
//! never pruned. This is the same marker, with the same contents.
//!
//! Like git's, the marker is written exclusively and without `fsync`: prune only asks whether the file
//! exists, so neither atomicity nor durability buys anything. It is removed on every exit once written —
//! explicitly by [`InitializingLock::release`] on success, and by `Drop` on an error, a panic, or the
//! future being dropped — so a failed create leaves the same `PartialRegistered` partial it always has.
//! Only a crash (no `Drop`) leaves it behind; the partial then reads as locked with reason
//! `initializing`, exactly as a killed `git worktree add` leaves its own.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::LinkedWorktreeError;

/// The marker's contents, as git writes them (`write_file` appends the newline).
const REASON: &[u8] = b"initializing\n";

/// A held `<admin>/locked` marker. `Drop` removes it (best-effort), so it never outlives the create
/// that wrote it unless the process dies.
pub(crate) struct InitializingLock {
	path: PathBuf,
	released: bool,
}

impl InitializingLock {
	/// Write `<admin>/locked` exclusively. An existing `locked` is never overwritten — it belongs to
	/// someone else (a concurrent git, or a user's lock), so finding one is an error and the guard is not
	/// created, leaving that marker alone.
	pub(crate) fn acquire(admin: &Path) -> Result<Self, LinkedWorktreeError> {
		let path = admin.join("locked");
		let mut file = std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
			.map_err(|e| LinkedWorktreeError::io("locking new admin dir", &path, e))?;
		// From here the file exists, so the guard owns it even if the write below fails.
		let lock = InitializingLock {
			path,
			released: false,
		};
		file
			.write_all(REASON)
			.map_err(|e| LinkedWorktreeError::io("locking new admin dir", &lock.path, e))?;
		Ok(lock)
	}

	/// Remove the marker now that the worktree is complete, reporting a failure: a marker left behind
	/// would make the finished worktree read as locked.
	pub(crate) fn release(mut self) -> Result<(), LinkedWorktreeError> {
		self.released = true;
		std::fs::remove_file(&self.path)
			.map_err(|e| LinkedWorktreeError::io("unlocking new admin dir", &self.path, e))
	}
}

impl Drop for InitializingLock {
	fn drop(&mut self) {
		if !self.released {
			// Best-effort: the create is already failing (or being dropped); its own error is the one
			// to report. A marker that survives this reads as a locked partial, never as a live worktree.
			let _ = std::fs::remove_file(&self.path);
		}
	}
}
