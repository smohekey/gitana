//! Reading a worktree git directory's `HEAD` and `locked` admin files.
//!
//! These are plain `std::fs` reads of files that live outside the object store, using the same ambient
//! authority the capability mint uses (absolute paths, never the process CWD). Lifted from the CLI's
//! `commands/worktree.rs`, re-typed onto [`LinkedWorktreeError`].

use std::path::Path;

use gitana_object::HashAlgorithm;
use gitana_repository::HeadState;

use crate::LinkedWorktreeError;
use crate::error::PointerKind;
use crate::facts::LockState;

/// Parse `<git_dir>/HEAD`. `Ok(None)` when the file is absent (no checkout / not a git dir). A **legacy
/// symlink** HEAD (`.git/HEAD -> refs/heads/main`) is symbolic — git resolves it that way — so its link
/// target is the branch, never followed to the branch's object id. A HEAD symlink whose target does *not*
/// name a ref (`HEAD -> oidfile`, `HEAD -> ../escape`) is one git rejects the repository over, so it is a
/// hard `MalformedPointer` error, never a resolvable symbolic HEAD.
pub(crate) fn read_head<H: HashAlgorithm>(
	git_dir: &Path,
) -> Result<Option<HeadState<H>>, LinkedWorktreeError> {
	let path = git_dir.join("HEAD");
	match std::fs::symlink_metadata(&path) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(LinkedWorktreeError::io("reading HEAD", path, error)),
		Ok(meta) if meta.file_type().is_symlink() => {
			let target = std::fs::read_link(&path)
				.map_err(|error| LinkedWorktreeError::io("reading HEAD symlink", path.clone(), error))?;
			if !target.to_string_lossy().starts_with("refs/") {
				return Err(LinkedWorktreeError::MalformedPointer {
					kind: PointerKind::Head,
					path,
				});
			}
			Ok(Some(HeadState::Symbolic(
				target.to_string_lossy().into_owned(),
			)))
		}
		Ok(_) => match std::fs::read(&path) {
			// `HeadState::parse` accepts both the spaced and no-space `ref:` symref forms git allows.
			Ok(bytes) => {
				HeadState::parse(&bytes)
					.map(Some)
					.map_err(|_| LinkedWorktreeError::MalformedPointer {
						kind: PointerKind::Head,
						path,
					})
			}
			Err(error) => Err(LinkedWorktreeError::io("reading HEAD", path, error)),
		},
	}
}

/// The lock state of a worktree whose git directory may hold a `locked` file. git writes the reason
/// (if any) as the file's contents. Only a genuine *absence* of the file is `Unlocked`; a file that
/// exists but cannot be read or decoded is kept `Locked` (with an unavailable reason), so a lock is
/// never silently dropped — the protective state is preserved even when the reason is unreadable.
///
/// **This deliberately diverges from git, and the divergence is the point** (decided with Scott; see the
/// symlink section of `docs/hlds/linked-worktree-library.md`). Probed: point `<admin>/locked` at any file
/// and `git worktree list --porcelain` prints *that file's contents* as the reason — literally
/// `locked SUPER SECRET FILE CONTENTS`. git reads through the link, so anyone who can write inside `.git`
/// turns a read-only-looking listing into a file-disclosure primitive. `gta`'s own native `list` inherited
/// this (verified against the built binary) by mirroring git; this crate does not, and is the behaviour
/// `gta` should adopt. Not a high-severity hole — `.git` write access already implies code execution via
/// hooks — but there is no reason to reproduce it.
pub(crate) fn read_lock_reason(git_dir: &Path) -> LockState {
	let marker = git_dir.join("locked");
	// A `symlink_metadata` probe distinguishes a genuinely *absent* marker from one that exists but is
	// unreadable (incl. a dangling symlink). Only a true absence is `Unlocked`; anything present keeps the
	// lock. A **symlinked** marker is never followed — its target's contents must not be exposed as the
	// public lock reason — so it is `Locked` with an unavailable reason.
	match std::fs::symlink_metadata(&marker) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockState::Unlocked,
		Err(_) => LockState::Locked { reason: None },
		Ok(meta) if meta.file_type().is_symlink() => LockState::Locked { reason: None },
		Ok(_) => match std::fs::read_to_string(&marker) {
			Ok(reason) => LockState::Locked {
				reason: Some(reason.trim().to_owned()),
			},
			Err(_) => LockState::Locked { reason: None },
		},
	}
}
