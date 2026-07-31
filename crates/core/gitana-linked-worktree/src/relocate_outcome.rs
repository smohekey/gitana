//! The structured result of a successful [`relocate`](crate::relocate).

use std::path::PathBuf;

/// The outcome of a [`relocate`](crate::relocate) that did not fail — either the worktree's checkout was
/// moved, or it was already at the requested path (an idempotent no-op). Refusals (locked / primary /
/// identity mismatch / occupied destination) and hard failures are [`RelocateError`](crate::RelocateError),
/// never an outcome here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocateOutcome {
	/// The worktree's checkout directory was moved from `from` to `to`. Its admin directory (and `git
	/// worktree` id), branch, and commits are **unchanged** — only the checkout's path moved.
	Relocated {
		/// The path the checkout moved from.
		from: PathBuf,
		/// The path the checkout now lives at.
		to: PathBuf,
	},
	/// `from` and `to` name the same path — the worktree is already where the request asks. Idempotent no-op.
	AlreadyAt {
		/// The path the worktree already occupies.
		to: PathBuf,
	},
}
