//! The outcome of a refused or failed [`create`](crate::create).

use crate::{LinkedWorktreeError, WorktreeClassification, WorktreeInspection};

/// Why a [`create`](crate::create) did not establish the requested worktree.
///
/// A create either succeeds (returning the resulting `WorktreeInspection` — including when it already
/// existed exactly, an idempotent no-op) or fails with this error. Refusals — states that block a safe
/// create — are matchable; genuine I/O / repository failures are [`Failed`](CreateError::Failed).
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
	/// A shared conflict/protection state (from the read-model vocabulary — `DestinationConflict`,
	/// `BranchUseConflict`, `IdentityConflict`, `PartialConflicting`, `PartialRegistered`,
	/// `ProtectedWithReason`) blocks the create. Never a success/idempotent classification.
	#[error("create refused: {0:?}")]
	Refused(WorktreeClassification),

	/// The destination already holds a live worktree that does **not** match the request (a different
	/// checkout mode, branch, or commit). Carries what is actually there.
	#[error("destination already holds a different worktree")]
	ExistingWorktreeMismatch(Box<WorktreeInspection>),

	/// A `NewBranch` / `Orphan` target requires the branch to be absent, but it already exists (at a point
	/// other than an interrupted create's start).
	#[error("branch already exists: {0}")]
	BranchExists(String),

	/// An `ExistingBranch` target requires the branch to exist, but it does not.
	#[error("branch not found: {0}")]
	BranchNotFound(String),

	/// The requested branch name is invalid per git's `check-ref-format --branch`.
	#[error("invalid branch name: {0}")]
	InvalidBranchName(String),

	/// A hard failure while inspecting or writing (I/O, a malformed pointer, a repository/ref error).
	#[error(transparent)]
	Failed(#[from] LinkedWorktreeError),
}
