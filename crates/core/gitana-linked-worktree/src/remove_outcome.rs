//! The structured result of a successful [`remove`](crate::remove).

use std::path::PathBuf;

/// The outcome of a [`remove`](crate::remove) that did not fail — either the worktree was removed, or it was
/// already absent (an idempotent no-op). Refusals (dirty/locked/primary/identity mismatch) and hard failures
/// are [`RemoveError`](crate::RemoveError), never an outcome here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
	/// The linked worktree was removed. Its checkout directory and administrative entry are gone; its
	/// **branch and commits are retained** (removal never deletes a ref). This also covers cleaning a
	/// recoverable partial (a registration whose checkout was interrupted mid-create): the retained admin and
	/// any attributable leftover checkout files are removed so a retry sees an absent destination.
	Removed {
		/// The destination that was removed.
		destination: PathBuf,
		/// The branch the removed worktree carried (retained), or `None` if it was detached/unborn.
		retained_branch: Option<String>,
	},
	/// The exact linked worktree is already absent — nothing to remove. Idempotent: a repeated removal after
	/// the worktree is gone reports this rather than failing.
	AlreadyAbsent {
		/// The destination that was found already absent.
		destination: PathBuf,
	},
}
