//! The explicit input to [`remove`](crate::remove) — the safe, force-free removal Code Henge uses.

use std::path::PathBuf;

use crate::query::BranchName;
use crate::{RemovePolicy, RepositoryId};

/// A request to remove one linked worktree at `destination` from `repo`.
///
/// The identity is explicit (the shared common dir via [`RepositoryId`], and the destination path) and is
/// **re-verified immediately before any destructive effect**. `expected_branch`, when set, additionally
/// pins the branch the destination must carry — a worktree registered to a *different* branch is refused as
/// an identity mismatch rather than removed. `remove` never deletes a branch or its commits.
///
/// `policy` selects the removal semantics: the default [`Conservative`](RemovePolicy::Conservative) is the
/// safe, force-free Code Henge surface (refuses a dirty/conflicted/locked/primary/identity-mismatched
/// worktree); [`GitCompat`](RemovePolicy::GitCompat) with `force >= 1` is git's `worktree remove -f` /
/// `-f -f` — a separate structural path that skips the cleanliness check but still validates `.git`
/// integrity. `GitCompat { force: 0 }` behaves as `Conservative`.
#[derive(Debug, Clone)]
pub struct RemoveRequest {
	/// The repository to remove the worktree from (explicit identity, anchored on the shared common dir).
	pub repo: RepositoryId,
	/// The checkout path to remove (absolute; never an identity source).
	pub destination: PathBuf,
	/// The branch the caller expects the destination to carry. `Some` pins the identity so a mismatch is
	/// refused; `None` removes whatever exact worktree lives there (its cross-pointer identity still checked).
	pub expected_branch: Option<BranchName>,
	/// How hard to push past the safe refusals — [`Conservative`](RemovePolicy::Conservative) (the safe
	/// default) or [`GitCompat`](RemovePolicy::GitCompat) with git's repeatable-`-f` force.
	pub policy: RemovePolicy,
}
