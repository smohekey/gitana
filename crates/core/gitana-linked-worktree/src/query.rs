//! The inputs to a read-only worktree query.

use std::path::PathBuf;

use crate::{RepositoryId, WorktreeObjectId};

/// A short local branch name (the `<name>` in `refs/heads/<name>`).
///
/// Slice 1 only *reads* branch state, so this is a thin wrapper that formats the ref name and compares.
/// Full git `check-ref-format` validation (rejecting malformed names before a change) is enforced where
/// a branch is *created* — the create slice — not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchName(String);

impl BranchName {
	/// Wrap a short branch name (e.g. `feature`, `topic/x`). No leading `refs/heads/`.
	pub fn new(name: impl Into<String>) -> Self {
		BranchName(name.into())
	}

	/// The short form as given.
	pub fn short(&self) -> &str {
		&self.0
	}

	/// The fully-qualified ref name, `refs/heads/<name>`.
	pub fn refname(&self) -> String {
		format!("refs/heads/{}", self.0)
	}
}

/// A read-only query about one destination against one explicitly-identified repository.
///
/// The repository is named by [`RepositoryId`] (anchored on the shared common dir); the destination is
/// a *query argument*, never the identity — so ownership is never inferred from the destination path.
#[derive(Debug, Clone)]
pub struct WorktreeQuery {
	/// The repository this query is about (explicit identity).
	pub repo: RepositoryId,
	/// The checkout path to inspect (native path; not an identity source).
	pub destination: PathBuf,
	/// The branch the caller expects this destination to carry. Drives the requested-branch facts and
	/// identity-conflict detection. `None` inspects the destination without a branch expectation.
	pub expected_branch: Option<BranchName>,
	/// The commit the caller intends the worktree's branch to sit at. When set (a reconciliation intent),
	/// inspection computes the ancestry relation between it and the worktree's current object — so
	/// `classify` can tell a genuinely *advanced* branch (start is an ancestor) from one **rewound or
	/// diverged** onto unrelated history (a conflict), rather than calling any differing object "advanced".
	/// `None` means no start expectation (pure inspection).
	pub start: Option<WorktreeObjectId>,
	/// Whether inspection should also compute the working-tree **status** of a live checkout (the cost of a
	/// full worktree scan), populating [`WorktreeInspection::status`](crate::WorktreeInspection::status) so
	/// `classify` can report a dirty/conflicted worktree as `ProtectedWithReason::Dirty`. `false` (the
	/// default for a pure inspection) skips the scan — status is only needed before a cleanup decision
	/// (removal), so a plain `inspect` stays cheap. Only a present, cross-pointer-consistent live checkout is
	/// statused; a missing/partial checkout has no status to compute.
	pub with_status: bool,
}
