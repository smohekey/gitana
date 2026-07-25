//! The outcome of a refused or failed [`relocate`](crate::relocate).

use std::path::PathBuf;

use crate::inspect::DestinationKind;
use crate::{LinkedWorktreeError, WorktreeClassification, WorktreeInspection};

/// Why a [`relocate`](crate::relocate) did not move the requested worktree.
///
/// A relocate either succeeds (a [`RelocateOutcome`](crate::RelocateOutcome)) or fails with this error.
/// Refusals — states the safe surface will not act on — carry a matchable observation; genuine I/O /
/// repository failures are [`Failed`](RelocateError::Failed).
#[derive(Debug, thiserror::Error)]
pub enum RelocateError {
	/// `from` is not a live, consistent linked worktree of this repository that can be moved. It is locked
	/// (carried as `ProtectedWithReason`), registered to a branch other than the pinned `expected_branch` or
	/// otherwise identity-conflicted, a recoverable/foreign partial, or simply absent — the shared
	/// [`WorktreeClassification`] says which. Never a success classification.
	#[error("relocate refused: {0:?}")]
	Refused(WorktreeClassification),

	/// `from` is the repository's **primary/main** worktree — git's `worktree move` refuses it ("is a main
	/// working tree"), and so does this safe surface. Carries `from`.
	#[error("cannot relocate the primary working tree: {}", .0.display())]
	IsPrimaryWorktree(PathBuf),

	/// Something already occupies `to` on disk — the move never overwrites it (git: "target already exists").
	/// Carries the destination and what sits there.
	#[error("relocate refused: destination {} already exists ({kind:?})", .path.display())]
	DestinationOccupied {
		/// The occupied destination path.
		path: PathBuf,
		/// What sits at the destination.
		kind: DestinationKind,
	},

	/// Another worktree registration already names `to` (a live or a checkout-missing/prunable admin). Moving
	/// there would leave two admin directories naming one checkout — git would list it twice and later
	/// inspection would report a duplicate registration — so it is refused unless `force` permits dropping the
	/// stale registration. `required_force` is the force that *would* have permitted it (2 when any stale
	/// admin is locked, else 1), computed with the byte-clean, no-follow lock reader — carried so a caller
	/// reports the right `-f`/`-f -f` guidance without re-probing the admin's lock file.
	#[error("relocate refused: destination {} is already a registered worktree ({}); needs force {required_force}", .path.display(), .admin_dir.display())]
	DestinationRegistered {
		/// The destination already claimed by a registration.
		path: PathBuf,
		/// The admin directory that claims it (the first, when several do).
		admin_dir: PathBuf,
		/// The `force` that would permit dropping the stale registration(s): 2 if any is locked, else 1.
		required_force: u8,
	},

	/// `from` **encloses the repository's own git storage** (its shared common dir lives inside the checkout —
	/// a `--separate-git-dir`/relocated-bare topology). Moving `from` would relocate the repository, the admin
	/// directory, and the held registration lock along with it, stranding them and failing the pointer rewrite
	/// — so it is refused before the rename. Carries the common dir found inside `from`.
	#[error("cannot relocate a worktree that encloses the repository git dir: {}", .0.display())]
	EnclosesRepository(PathBuf),

	/// A move step ran but the worktree is now neither fully at `from` nor fully at `to` — a partial move (or
	/// a concurrent change) a caller must inspect and retry. Carries the observed post-state of `from`. (A
	/// relocate never reports success for a state it did not fully establish.)
	#[error("relocate did not complete cleanly; re-inspect the worktree")]
	Incomplete(Box<WorktreeInspection>),

	/// A hard failure while inspecting or moving (I/O, a malformed pointer, or a repository error).
	#[error(transparent)]
	Failed(#[from] LinkedWorktreeError),
}
