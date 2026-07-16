//! The outcome of a refused or failed [`remove`](crate::remove).

use std::path::PathBuf;

use crate::{LinkedWorktreeError, WorktreeClassification, WorktreeInspection};

/// Why a [`remove`](crate::remove) did not remove the requested worktree.
///
/// A remove either succeeds (a [`RemoveOutcome`](crate::RemoveOutcome)) or fails with this error. Refusals —
/// states the safe surface will not act on — are matchable observations; genuine I/O / repository failures
/// are [`Failed`](RemoveError::Failed).
#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
	/// A shared conflict/protection state blocks removal (from the read-model vocabulary):
	/// `ProtectedWithReason` (locked, or a **dirty/conflicted** live checkout — carrying the status report),
	/// `IdentityConflict` (a cross-pointer disagreement, a duplicate registration, a foreign `.git`, or a
	/// branch other than the pinned `expected_branch`), `DestinationConflict` (unrelated content the removal
	/// must not touch), or `PartialConflicting` (a checkout not registered to this repository). Never a
	/// success classification.
	#[error("remove refused: {0:?}")]
	Refused(WorktreeClassification),

	/// The destination is the repository's **primary/main** worktree — the safe surface never removes it (git:
	/// "is a main working tree"). Carries the destination.
	#[error("cannot remove the primary working tree: {}", .0.display())]
	IsPrimaryWorktree(PathBuf),

	/// The destination **encloses the repository's own git storage** (its shared common dir lives inside the
	/// checkout, e.g. a bare repo relocated to `<destination>/meta.git`). Recursively deleting the checkout
	/// would destroy the repository's refs and objects — including the branch removal must retain — so it is
	/// refused unconditionally. Carries the common dir found inside the destination.
	#[error("cannot remove a worktree that encloses the repository git dir: {}", .0.display())]
	EnclosesRepository(PathBuf),

	/// A destructive step ran but the worktree is now neither fully present nor fully removed — a partial
	/// removal (or a concurrent change) that a caller must inspect and retry. Carries the observed post-state.
	/// (A remove never reports success for a state it did not fully establish.)
	#[error("remove did not complete cleanly; re-inspect the destination")]
	Incomplete(Box<WorktreeInspection>),

	/// A hard failure while inspecting or removing (I/O, a malformed pointer, a repository/ref error, or a
	/// status computation that could not be completed — which is never silently treated as clean).
	#[error(transparent)]
	Failed(#[from] LinkedWorktreeError),
}
