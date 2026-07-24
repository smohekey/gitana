//! The explicit input to [`relocate`](crate::relocate) — the safe move of a linked worktree's checkout.

use std::path::PathBuf;

use crate::RepositoryId;
use crate::query::BranchName;

/// A request to move the linked worktree at `from` to `to`, within `repo` — mirroring `git worktree move`.
///
/// The identity is explicit (the shared common dir via [`RepositoryId`], and the `from` path) and is
/// **re-verified immediately before the move**. `expected_branch`, when set, additionally pins the branch
/// the worktree must carry — a worktree registered to a *different* branch is refused as an identity
/// mismatch rather than moved. The move preserves the worktree's identity: its admin directory (and thus
/// its `git worktree` id), branch, and commits are unchanged — only the checkout's path changes.
///
/// `to`'s parent directories must already exist and nothing may occupy `to` itself: this is a faithful
/// `git worktree move`, so it does not create intermediate directories (the caller owns its path layout).
#[derive(Debug, Clone)]
pub struct RelocateRequest {
	/// The repository the worktree belongs to (explicit identity, anchored on the shared common dir).
	pub repo: RepositoryId,
	/// The worktree's current checkout path (absolute; only used to locate the worktree, never an identity
	/// source beyond that).
	pub from: PathBuf,
	/// The new checkout path (absolute). Its parent must exist; nothing may already occupy it.
	pub to: PathBuf,
	/// The branch the caller expects the worktree to carry. `Some` pins the identity so a mismatch is refused;
	/// `None` moves whatever exact worktree lives at `from` (its cross-pointer identity still checked).
	pub expected_branch: Option<BranchName>,
}
