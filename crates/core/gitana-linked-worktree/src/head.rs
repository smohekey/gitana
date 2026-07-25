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
use crate::pointers::strip_eol_bytes;

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

/// A **no-follow, structural** read of `<git_dir>/HEAD` for the forced-removal validity gate — the shapes
/// git accepts even under `-f -f`. Returns:
///
/// - `Some(Some(branch))` — a **valid symbolic** HEAD naming the **direct** ref `branch` (`refs/...`,
///   unpeeled): a `ref: refs/...` content file (spaced or not), or a **legacy filesystem symlink** whose
///   *link target* names a ref. The symlink's target is read with `read_link` — never followed to the
///   pointed-at file — so a crafted `HEAD -> refs/...` cannot disclose an external file's contents, and the
///   ref is never resolved (a corrupt/cyclic branch still validates, as `git -f` removes such a worktree).
/// - `Some(None)` — a **valid detached** HEAD: a bare, well-formed object id (40-hex SHA-1 or 64-hex
///   SHA-256). Checked length/charset only, so it is object-format-agnostic and reads no object store.
/// - `None` — **structurally invalid**: an absent/unreadable/empty/malformed HEAD, a directory, a non-ref
///   symlink, a `ref:` target not under `refs/`, or a hex string of the wrong length. git refuses these
///   under `-f -f`, so the forced path must not delete such a worktree.
pub(crate) fn structural_head_branch(git_dir: &std::path::Path) -> Option<Option<String>> {
	// A detached HEAD is a bare object id with NO surrounding whitespace — git rejects a space/tab-padded one
	// under `-f -f`. `text` is already stripped of trailing line terminators; do *not* additionally trim, so
	// `"  <hex>  "` fails the exact length/charset check and is invalid.
	structural_head_branch_with(git_dir, |raw| {
		matches!(raw.len(), 40 | 64) && raw.iter().all(u8::is_ascii_hexdigit)
	})
}

/// Like [`structural_head_branch`], but with the **looser detached grammar `git worktree move` accepts**:
/// a valid object-id *prefix* of the repository's hash width (`hexsz` hex chars — 40 for SHA-1, 64 for
/// SHA-256), with any trailing content ignored — where the force-removal gate requires the *exact*,
/// unpadded id. Probed against git 2.50.1: `move` accepts `<40 hex> trailing`, `<40 hex>AA…` (overlong), and
/// the bare id, but refuses an absent/empty/garbage/short/whitespace-padded HEAD, and a symbolic HEAD only
/// when it names a full `refs/...`. Symbolic-HEAD handling is identical to [`structural_head_branch`].
pub(crate) fn structural_head_branch_for_move(
	git_dir: &std::path::Path,
	hexsz: usize,
) -> Option<Option<String>> {
	structural_head_branch_with(git_dir, move |raw| {
		// git parses the leading object-id of the repository's width and ignores whatever follows — even a
		// non-UTF-8 trailing byte, so the detached form is matched on **raw bytes**, never a UTF-8 decode of
		// the whole file. A shorter run, or any non-hex within the first `hexsz` bytes (e.g. leading
		// whitespace), is not a valid id.
		raw
			.get(..hexsz)
			.is_some_and(|prefix| prefix.iter().all(u8::is_ascii_hexdigit))
	})
}

/// Shared structural HEAD read; `is_valid_detached` decides which non-`ref:` (detached) **raw byte** texts
/// are accepted, the only axis on which the force-removal and move gates differ. The detached form is
/// matched on bytes (never a UTF-8 decode of the whole file) so a valid hex id with a non-UTF-8 trailing
/// byte — which git's fixed-width parse accepts — is not lost; only a *symbolic* target is decoded (a ref
/// name that must be valid UTF-8 to return as a `String`).
fn structural_head_branch_with(
	git_dir: &std::path::Path,
	is_valid_detached: impl Fn(&[u8]) -> bool,
) -> Option<Option<String>> {
	let path = git_dir.join("HEAD");
	match std::fs::symlink_metadata(&path) {
		// A legacy symbolic-ref HEAD is a filesystem symlink whose *link target* names a ref. Read the link
		// (no follow), so the target string is the ref name, never the pointed-at ref file's object-id content.
		Ok(meta) if meta.file_type().is_symlink() => {
			let target = std::fs::read_link(&path).ok()?;
			let target = target.to_string_lossy();
			target
				.starts_with("refs/")
				.then(|| Some(target.into_owned()))
		}
		Ok(meta) if meta.is_file() => {
			let bytes = std::fs::read(&path).ok()?;
			// Parse the HEAD file's own bytes structurally — the same grammar as `HeadState::parse` (trim only
			// trailing line terminators; `ref:` then space/tab), without an object-store read.
			let raw = strip_eol_bytes(&bytes);
			if let Some(rest) = raw.strip_prefix(b"ref:".as_slice()) {
				// git accepts space/tab (only) between `ref:` and the target, so trim those from the *symbolic*
				// target — but it is valid only when it then names a full ref (`refs/...`); `ref: main`, an empty
				// target, or a non-space/tab separator left in the target is not a HEAD to force past.
				let target = trim_spaces_and_tabs(rest);
				let target = std::str::from_utf8(target).ok()?;
				target.starts_with("refs/").then(|| Some(target.to_owned()))
			} else {
				is_valid_detached(raw).then_some(None)
			}
		}
		// Absent, a directory, or any stat/read failure — structurally invalid.
		_ => None,
	}
}

/// Trim leading and trailing space/tab bytes only (not other whitespace), matching git's `ref:` separator
/// grammar, on a raw byte slice.
fn trim_spaces_and_tabs(mut bytes: &[u8]) -> &[u8] {
	while let [first, rest @ ..] = bytes {
		if matches!(first, b' ' | b'\t') {
			bytes = rest;
		} else {
			break;
		}
	}
	while let [rest @ .., last] = bytes {
		if matches!(last, b' ' | b'\t') {
			bytes = rest;
		} else {
			break;
		}
	}
	bytes
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
