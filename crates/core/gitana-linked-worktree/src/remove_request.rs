//! The explicit input to [`remove`](crate::remove) — the safe, force-free removal Code Henge uses.

use std::path::PathBuf;

use crate::RepositoryId;
use crate::query::BranchName;

/// A request to safely remove one linked worktree at `destination` from `repo`.
///
/// The identity is explicit (the shared common dir via [`RepositoryId`], and the destination path) and is
/// **re-verified immediately before any destructive effect**. `expected_branch`, when set, additionally
/// pins the branch the destination must carry — a worktree registered to a *different* branch is refused as
/// an identity mismatch rather than removed. This safe surface has **no force mode**: it refuses a dirty,
/// conflicted, locked, primary, or identity-mismatched worktree, and never deletes a branch or its commits.
#[derive(Debug, Clone)]
pub struct RemoveRequest {
	/// The repository to remove the worktree from (explicit identity, anchored on the shared common dir).
	pub repo: RepositoryId,
	/// The checkout path to remove (absolute; never an identity source).
	pub destination: PathBuf,
	/// The branch the caller expects the destination to carry. `Some` pins the identity so a mismatch is
	/// refused; `None` removes whatever exact worktree lives there (its cross-pointer identity still checked).
	pub expected_branch: Option<BranchName>,
}
