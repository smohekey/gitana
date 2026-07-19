//! The explicit input to [`create`](crate::create) — no DWIM, no defaulting from the destination path.

use std::path::PathBuf;

use crate::query::BranchName;
use crate::{RepositoryId, WorktreeObjectId};

/// What a new linked worktree should check out. Every field is explicit — the caller (not this crate)
/// resolves any DWIM before asking. The four variants map 1:1 to git's `worktree add` modes, and each
/// **encodes the caller's intent** so a create is never silently a reconcile (or vice-versa):
///
/// - [`NewBranch`](CheckoutTarget::NewBranch) — git `-b`: create the branch at `start`.
/// - [`ExistingBranch`](CheckoutTarget::ExistingBranch) — git `<branch>`: check out an existing branch.
/// - [`Detached`](CheckoutTarget::Detached) — git `--detach`.
/// - [`Orphan`](CheckoutTarget::Orphan) — git `--orphan -b`: an unborn branch, empty checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
	/// Create branch `name` at `start` and check it out (git `worktree add -b`). The branch **must not
	/// already exist** — unless it sits at `start` with no worktree yet (an interrupted create), which is
	/// finished. A branch that exists at a *different* commit is a conflict (never silently reset) — unless
	/// `force_reset` is set (git `-B`), which resets an existing branch to `start` and checks it out.
	NewBranch {
		/// The short branch name (`refs/heads/<name>`).
		name: BranchName,
		/// The commit the new branch is created at.
		start: WorktreeObjectId,
		/// git `-B`: when the branch already exists, **reset** it to `start` (a compare-and-reset against
		/// its current tip, never a blind clobber) and check it out, rather than refusing it as
		/// [`CreateError::BranchExists`](crate::CreateError::BranchExists). `false` (the default) keeps strict
		/// `-b` semantics — an existing branch is a conflict. A branch checked out in *another* worktree is
		/// still refused either way (git refuses `-B` on an in-use branch too). A **symbolic-ref** branch
		/// (rare) is refused with [`CreateError::UnsupportedSymbolicBranchReset`](crate::CreateError::UnsupportedSymbolicBranchReset)
		/// rather than dereferenced — reset its terminal branch directly.
		force_reset: bool,
	},
	/// Check out the **existing** branch `name` at its current tip (git `worktree add <branch>`). The
	/// branch must already exist; it is never created here. When `expected_start` is set — for reconciling
	/// an interrupted create that expected the branch at a specific commit — the branch must be **at or
	/// descended from** that commit, else the request is refused rather than checking out divergent history.
	ExistingBranch {
		/// The short branch name to check out.
		name: BranchName,
		/// The commit the branch is expected to be at (or ahead of), when reconciling. `None` accepts the
		/// branch wherever it currently points (plain `git worktree add <branch>`).
		expected_start: Option<WorktreeObjectId>,
	},
	/// A detached `HEAD` at `start` (git `worktree add --detach`).
	Detached {
		/// The commit to check out and detach at.
		start: WorktreeObjectId,
	},
	/// An **orphan** worktree: `HEAD` points at the unborn branch `name` and the checkout is left empty
	/// (git `worktree add --orphan -b <name>`). The branch **must not already exist** (git refuses to
	/// orphan an existing branch); no ref is created and there is no start commit.
	Orphan {
		/// The short branch name the unborn `HEAD` points at.
		name: BranchName,
	},
}

/// A fully-explicit request to establish one linked worktree at `destination` in `repo`.
#[derive(Debug, Clone)]
pub struct CreateRequest {
	/// The repository to create the worktree in (explicit identity, anchored on the shared common dir).
	pub repo: RepositoryId,
	/// The checkout path to create (absolute; never an identity source).
	pub destination: PathBuf,
	/// What the new worktree checks out.
	pub target: CheckoutTarget,
}
