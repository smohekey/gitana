//! The partial-state classification — the requirements doc's Partial-State table as a matchable enum,
//! derived by a **pure, total** function over a [`WorktreeInspection`]. A classification is an
//! *observation*, never an error; it is what a create/remove caller matches on to decide safe-to-act
//! vs. conflict. `classify` performs no I/O.

use crate::WorktreeObjectId;
use crate::facts::{HeadKind, LockState};
use crate::inspect::{
	CrossPointerHealth, DestinationKind, IdentityConflict, Registration, RequestedBranch,
	StartRelation, WorktreeInspection,
};

/// Why a destination is protected from an automatic effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectionReason {
	/// The registration is locked; the recorded reason (if any) is carried.
	Locked {
		/// The lock reason, `Some("")` when locked without one.
		reason: Option<String>,
	},
	/// The live checkout has staged, unstaged, untracked, conflicted, or missing changes — removing it would
	/// discard user work. The full status report is carried so a caller can see exactly *why* (the report
	/// distinguishes conflicted/staged/unstaged/untracked/missing). Only reported when a status was requested
	/// (a removal decision); a pure inspection never runs a status and so never yields this.
	Dirty(Box<crate::WorktreeStatusReport>),
	/// The live checkout's working tree is *clean* in git's status sense but still holds **residual** files
	/// with no index entry — untracked *or* ignored content (build artifacts, a stray `.env`). Safe removal
	/// preserves it rather than recursively deleting (gitana's ignore matcher is not fully git-faithful, so a
	/// non-tracked file is never deleted on faith). The offending paths (a capped sample, worktree-relative)
	/// are carried so a caller knows what to clear before removal can proceed.
	ResidualContent {
		/// The residual (untracked/ignored) paths found, relative to the worktree, capped at a sample.
		paths: Vec<String>,
	},
	/// The live checkout is *clean* in git's status sense, but a re-verification that **hashes** every present
	/// tracked file (rather than trusting the index stat cache) found one whose content or mode diverges from the
	/// index. `status` can miss this — a same-size / stat-preserving rewrite, an edit within a coarse-timestamp
	/// filesystem's granularity — and it omits skip-worktree entries entirely. Either way removing the worktree
	/// would discard the edit, so safe removal preserves it; the offending paths are carried so a caller knows
	/// what to reconcile first.
	ModifiedTrackedContent {
		/// The present, content-diverged tracked paths, relative to the worktree.
		paths: Vec<String>,
	},
	/// The live checkout uses a **sparse index** (`git sparse-checkout --sparse-index`), whose collapsed
	/// `040000` sparse-directory entries gitana does not expand. A status computed over it reports spurious
	/// add/delete pairs, so removal cannot establish cleanliness safely; it refuses honestly rather than acting
	/// on that bogus status. This is a conservative *unsupported-state* refusal (no user data is necessarily at
	/// risk) — expanding sparse indexes is a deferred follow-up.
	SparseIndexUnsupported,
	/// A commit anchored **only** by something inside the worktree's admin dir — a **detached** (or
	/// per-worktree-symbolic) `HEAD`, or a `refs/worktree|bisect|rewritten/*` tip — is reachable from no
	/// surviving shared ref (`refs/heads`, `refs/tags`, `refs/remotes`, …). Removing the worktree drops that
	/// admin dir and would orphan the commit (later gc-able), so safe removal refuses, discharging the
	/// commit-preservation contract. The caller can create a branch or tag at the commit first, then retry. The
	/// orphaned commit is carried so a caller knows what to preserve.
	UnreachableAnchoredCommit {
		/// The commit that no surviving shared ref reaches.
		commit: WorktreeObjectId,
	},
	/// A **checkout-missing partial** (its checkout directory is gone, but the registration is retained) whose
	/// retained per-worktree index still holds **staged** (index-vs-`HEAD`) or **unmerged** work. Cleaning the
	/// partial would drop the admin dir and that index, erasing the staged state and leaving its index-only
	/// blobs unreferenced (gc-able). Safe removal refuses, so the caller can recover the staged work (or restore
	/// the checkout) first — matching how a live checkout's staged changes are a [`Dirty`](ProtectionReason::Dirty)
	/// refusal.
	StagedContentInMissingCheckout,
}

/// The classified state of a requested linked worktree, in the requirements doc's vocabulary. Returned
/// as `Ok` data — a refusal/conflict is an observation, not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeClassification {
	/// No branch, registration, or destination content — safe to create.
	AbsentSafeToCreate,
	/// The requested branch exists at the requested start, but no registration/checkout — a prior
	/// attempt that may be completed safely.
	InterruptedCompletable {
		/// The branch's object (equals the requested start).
		branch_object: WorktreeObjectId,
	},
	/// Registration and checkout match the request exactly.
	CompleteIdempotent {
		/// The checked-out branch ref.
		branch: String,
		/// Its current object (equals the requested start).
		object: WorktreeObjectId,
	},
	/// The exact worktree exists and its branch has advanced past the requested start — reported with
	/// its current object; the branch is **not** reset.
	MatchingAdvanced {
		/// The checked-out branch ref.
		branch: String,
		/// Its current (advanced) object.
		object: WorktreeObjectId,
	},
	/// A registered, cross-pointer-consistent, present worktree that is *not* an exact branch+object match
	/// — a healthy **detached** worktree (no branch) or **unborn/orphan** worktree (no object), as seen
	/// under pure inspection. Distinct from `CompleteIdempotent`/`MatchingAdvanced`, which report a
	/// worktree that matches a requested branch at (or past) a requested start. Not a conflict: the
	/// worktree is intact, just not describable as "on branch X at object Y".
	CompletePresent {
		/// The checked-out branch ref, when `HEAD` is on a branch (`None` when detached).
		branch: Option<String>,
		/// The current `HEAD` object, when it resolves (`None` when the branch is unborn).
		object: Option<WorktreeObjectId>,
		/// Whether `HEAD` is symbolic, detached, or unborn.
		head: HeadKind,
	},
	/// The registration is retained but the checkout is missing.
	PartialRegistered {
		/// The admin directory whose checkout has gone.
		admin_dir: std::path::PathBuf,
	},
	/// A checkout exists but its registration is missing or inconsistent.
	PartialConflicting {
		/// The observed cross-pointer health.
		detail: CrossPointerHealth,
	},
	/// The requested branch is checked out at another destination.
	BranchUseConflict {
		/// The other worktree's checkout.
		other_checkout: std::path::PathBuf,
	},
	/// The destination holds unrelated content or a non-directory.
	DestinationConflict {
		/// What sits at the destination.
		kind: DestinationKind,
	},
	/// A concrete identity/integrity conflict.
	IdentityConflict {
		/// The conflict detail.
		detail: IdentityConflict,
	},
	/// The destination is protected — locked, or a live checkout that is dirty/conflicted — so an automatic
	/// effect (notably removal) is refused with the reason reported.
	ProtectedWithReason {
		/// Why it is protected.
		reason: ProtectionReason,
	},
}

/// Classify `inspection` — a pure, total, no-I/O function over the already-built inspection. The
/// requested start commit and its ancestry relation to the worktree's current object both live on the
/// inspection (computed by [`inspect`](crate::inspect)), so a fast-forward is distinguished from a
/// rewind/divergence here without any reachability walk.
///
/// Precedence is most-specific-refusal-first, so the returned variant is the one a caller must act on.
/// The [`StagedContentInMissingCheckout`](ProtectionReason::StagedContentInMissingCheckout) protection, if a
/// checkout-missing partial's retained index holds staged/unmerged work. `None` outside a removal decision or
/// for a live checkout. Shared by the two partial decision paths so both agree with `decide_remove`.
fn partial_staged_protection(inspection: &WorktreeInspection) -> Option<WorktreeClassification> {
	(inspection.partial_staged_changes == Some(true)).then_some(
		WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::StagedContentInMissingCheckout,
		},
	)
}

/// The [`UnreachableAnchoredCommit`](ProtectionReason::UnreachableAnchoredCommit) protection, if removal would
/// orphan a commit anchored only inside this worktree's admin dir (a detached/per-worktree-symbolic `HEAD`, or
/// a `refs/worktree|bisect|rewritten/*` tip). `None` outside a removal decision (`unreachable_admin_anchor` is
/// then `None`) or when every anchor is preserved — so create-time classification is never affected. Shared by
/// the partial and live decision paths so both agree with `decide_remove`.
fn unreachable_head_protection(inspection: &WorktreeInspection) -> Option<WorktreeClassification> {
	inspection.unreachable_admin_anchor.as_ref().map(|commit| {
		WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::UnreachableAnchoredCommit {
				commit: commit.clone(),
			},
		}
	})
}

pub fn classify(inspection: &WorktreeInspection) -> WorktreeClassification {
	use WorktreeClassification as C;
	let start = inspection.start.as_ref();

	// 1. Protected (locked) — a lock resists every automatic effect.
	if let LockState::Locked { reason } = &inspection.lock {
		return C::ProtectedWithReason {
			reason: ProtectionReason::Locked {
				reason: reason.clone(),
			},
		};
	}

	// 1a. A **recoverable mid-checkout partial**: an owned registration whose checkout is gone
	//     (`PresentCheckoutMissing` — the admin's `commondir` names this repository and its `gitdir` records
	//     this destination, git's own "prunable" attribution) **and the destination is absent or an empty
	//     directory** (nothing unknown sits there). Report it as `PartialRegistered` — a recoverable state a
	//     caller prunes-and-retries. The empty/absent gate matters: a *non-empty* directory at the recorded
	//     path is not verifiably this worktree's own content (a reused path, or a git-created prunable), so it
	//     is left to read as a `DestinationConflict` below rather than a false "recoverable". Also gated to no
	//     *more-specific* refusal — an identity conflict, or the requested branch being force-checked-out in
	//     another worktree (a `BranchUseConflict`; a retry stays blocked by that checkout, so "prune and retry"
	//     would mislead). This is the read side of the deferred slice-2 "recoverable mid-checkout" item.
	if inspection.identity_conflict.is_none()
		&& matches!(
			inspection.destination_kind,
			DestinationKind::Absent | DestinationKind::EmptyDir
		) && let Registration::PresentCheckoutMissing { admin_dir } = &inspection.registration
	{
		if let Some(other) = requested_checked_out_elsewhere(&inspection.requested_branch) {
			return C::BranchUseConflict {
				other_checkout: other.to_path_buf(),
			};
		}
		// A checkout-missing partial still holds the admin's index and `HEAD`. Cleaning it (dropping the admin)
		// would erase staged/unmerged index work, or orphan a detached / per-worktree-symbolic commit reachable
		// from no shared ref — so refuse first, in the same order as `decide_remove` (staged, then reachability),
		// rather than let "prune and retry" silently discard it.
		if let Some(protection) = partial_staged_protection(inspection) {
			return protection;
		}
		if let Some(protection) = unreachable_head_protection(inspection) {
			return protection;
		}
		return C::PartialRegistered {
			admin_dir: admin_dir.clone(),
		};
	}

	// 2. Destination holds a non-directory or unrelated content — never a completion candidate.
	if matches!(
		inspection.destination_kind,
		DestinationKind::OtherFsObject | DestinationKind::UnrelatedContent
	) {
		return C::DestinationConflict {
			kind: inspection.destination_kind.clone(),
		};
	}

	// 3. A start-independent identity conflict (cross-pointer disagreement outranks path equality).
	if let Some(detail) = &inspection.identity_conflict {
		return C::IdentityConflict {
			detail: detail.clone(),
		};
	}

	// 4. The requested branch is checked out at another destination — whether it exists or is unborn
	//    (`worktree add --orphan`), both of which git refuses to check out a second time.
	if let Some(other) = requested_checked_out_elsewhere(&inspection.requested_branch) {
		return C::BranchUseConflict {
			other_checkout: other.to_path_buf(),
		};
	}

	// 5. A checkout exists with a missing registration — partial, conflicting.
	if inspection.destination_kind == DestinationKind::LinkedWorktreeCheckout
		&& inspection.registration == Registration::None
	{
		return C::PartialConflicting {
			detail: inspection.cross_pointers.clone(),
		};
	}

	// 6. Registration retained, checkout gone.
	if let Registration::PresentCheckoutMissing { admin_dir } = &inspection.registration {
		// A checkout-missing partial still holds the admin's index and `HEAD`. Cleaning it (dropping the admin)
		// would erase staged/unmerged index work, or orphan a detached / per-worktree-symbolic commit reachable
		// from no shared ref — so refuse first, in the same order as `decide_remove` (staged, then reachability),
		// rather than let "prune and retry" silently discard it.
		if let Some(protection) = partial_staged_protection(inspection) {
			return protection;
		}
		if let Some(protection) = unreachable_head_protection(inspection) {
			return protection;
		}
		return C::PartialRegistered {
			admin_dir: admin_dir.clone(),
		};
	}

	// 7. Registered + consistent + a readable HEAD: a present, healthy worktree. When it is on a branch
	//    with a resolved object, the requested start decides: no start (or an exact-object start) is
	//    `CompleteIdempotent`; a start that is a proper *ancestor* is `MatchingAdvanced` (a fast-forward,
	//    reported, never reset); a *diverged*/rewound start (or one whose hash format bears no relation) is
	//    a conflict, not a spurious "advanced". A wrong branch was already caught as an `identity_conflict`
	//    above (compared by terminal ref). A **detached** (no branch) or **unborn** (no object) worktree is
	//    still healthy and present, just not an exact branch+object match — `CompletePresent`.
	if let Registration::Present { .. } = &inspection.registration
		&& inspection.cross_pointers == CrossPointerHealth::Consistent
		&& let Some(head) = &inspection.head
	{
		// A present, consistent, live checkout whose status was computed (a removal decision) may be protected.
		// This mirrors `decide_remove` exactly, so `classify(inspect(...))` agrees with the removal outcome:
		// **tracked-side** changes (staged/unstaged/conflicted/missing) → `Dirty`; otherwise any **residual**
		// (untracked *or ignored*) content → `ResidualContent` with its paths. Both out-rank the
		// exact/advanced/present readings below (the protection is the fact a cleanup caller acts on). A pure
		// inspection carries no status/residual, so neither fires for it.
		// A sparse index yields a bogus status (unexpanded `040000` entries → spurious add/delete pairs), so it
		// out-ranks the status-derived gates below: refuse honestly rather than trust that status.
		if inspection.sparse_index == Some(true) {
			return C::ProtectedWithReason {
				reason: ProtectionReason::SparseIndexUnsupported,
			};
		}
		if let Some(status) = &inspection.status
			&& status.has_tracked_changes()
		{
			return C::ProtectedWithReason {
				reason: ProtectionReason::Dirty(Box::new(status.clone())),
			};
		}
		// A present tracked file whose hash diverges from the index is a tracked-side edit `status` can miss
		// (stat-cache/skip-worktree) — still data loss if deleted.
		if let Some(paths) = &inspection.diverged_tracked_content
			&& !paths.is_empty()
		{
			return C::ProtectedWithReason {
				reason: ProtectionReason::ModifiedTrackedContent {
					paths: paths.clone(),
				},
			};
		}
		if let Some(residual) = &inspection.residual_paths
			&& !residual.is_empty()
		{
			return C::ProtectedWithReason {
				reason: ProtectionReason::ResidualContent {
					paths: residual.clone(),
				},
			};
		}
		// A clean checkout whose HEAD commit is reachable from no shared ref, and whose only anchor (a detached
		// HEAD, or a per-worktree symbolic target) will not survive removal, would be orphaned — refuse, so the
		// caller can branch/tag it first. (A HEAD symbolic to a surviving `refs/heads/*` branch is `Some(true)`.)
		if let Some(protection) = unreachable_head_protection(inspection) {
			return protection;
		}
		if let (Some(branch), Some(object)) = (&head.branch, &head.object) {
			let complete = || C::CompleteIdempotent {
				branch: branch.clone(),
				object: object.clone(),
			};
			return match (start, inspection.start_relation) {
				(None, _) | (Some(_), Some(StartRelation::Equal)) => complete(),
				(Some(_), Some(StartRelation::Ancestor)) => C::MatchingAdvanced {
					branch: branch.clone(),
					object: object.clone(),
				},
				// Diverged/rewound history, or a start of an unrelated hash format — never a safe advance.
				(Some(_), _) => C::IdentityConflict {
					detail: IdentityConflict::BranchAtUnexpectedObject {
						found: object.clone(),
					},
				},
			};
		}
		return C::CompletePresent {
			branch: head.branch.clone(),
			object: head.object.clone(),
			head: head.state,
		};
	}

	// 8. The requested branch exists at an object other than the requested start, and this destination
	//    does not already own it — a create-blocking identity conflict.
	if let RequestedBranch::Exists { object, .. } = &inspection.requested_branch
		&& inspection.registration == Registration::None
		&& start.is_some()
		&& start != Some(object)
	{
		return C::IdentityConflict {
			detail: IdentityConflict::BranchAtUnexpectedObject {
				found: object.clone(),
			},
		};
	}

	// 9. An interrupted attempt: branch at the requested start, nothing else yet.
	if inspection.registration == Registration::None
		&& matches!(
			inspection.destination_kind,
			DestinationKind::Absent | DestinationKind::EmptyDir
		) && let RequestedBranch::Exists { object, .. } = &inspection.requested_branch
		&& (start.is_none() || start == Some(object))
	{
		return C::InterruptedCompletable {
			branch_object: object.clone(),
		};
	}

	// 10. Nothing present and no conflicting branch — safe to create.
	if inspection.registration == Registration::None
		&& matches!(
			inspection.destination_kind,
			DestinationKind::Absent | DestinationKind::EmptyDir
		) && matches!(
		inspection.requested_branch,
		RequestedBranch::NotRequested | RequestedBranch::Absent { .. }
	) {
		return C::AbsentSafeToCreate;
	}

	// Defensive default: an unclassified residue is treated as conflicting rather than safe to create.
	C::PartialConflicting {
		detail: inspection.cross_pointers.clone(),
	}
}

/// The checkout of *another* worktree that carries the requested branch (a branch-use conflict), if any —
/// whether the branch exists or is an unborn orphan checked out elsewhere.
fn requested_checked_out_elsewhere(requested: &RequestedBranch) -> Option<&std::path::Path> {
	match requested {
		RequestedBranch::Exists {
			checked_out_elsewhere,
			..
		}
		| RequestedBranch::Absent {
			checked_out_elsewhere,
		} => checked_out_elsewhere.as_deref(),
		RequestedBranch::NotRequested => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::HeadKind;
	use crate::inspect::HeadFacts;
	use gitana_object::HashKind;
	use std::path::PathBuf;

	fn oid(hex: &str) -> WorktreeObjectId {
		WorktreeObjectId::parse(HashKind::Sha1, hex).unwrap()
	}

	fn base() -> WorktreeInspection {
		WorktreeInspection {
			destination: PathBuf::from("/dest"),
			expected_branch: None,
			destination_kind: DestinationKind::Absent,
			registration: Registration::None,
			cross_pointers: CrossPointerHealth::NotApplicable,
			git_dir: None,
			common_dir: PathBuf::from("/repo/.git"),
			head: None,
			requested_branch: RequestedBranch::NotRequested,
			start: None,
			start_relation: None,
			lock: LockState::Unlocked,
			identity_conflict: None,
			status: None,
			residual_paths: None,
			diverged_tracked_content: None,
			sparse_index: None,
			unreachable_admin_anchor: None,
			partial_staged_changes: None,
		}
	}

	#[test]
	fn absent_with_no_branch_is_safe_to_create() {
		assert!(matches!(
			classify(&base()),
			WorktreeClassification::AbsentSafeToCreate
		));
	}

	#[test]
	fn a_lock_outranks_a_destination_conflict() {
		// Both protected (locked) and unrelated content — the lock must win (highest precedence).
		let inspection = WorktreeInspection {
			destination_kind: DestinationKind::UnrelatedContent,
			lock: LockState::Locked {
				reason: Some("busy".to_owned()),
			},
			..base()
		};
		assert!(matches!(
			classify(&inspection),
			WorktreeClassification::ProtectedWithReason {
				reason: ProtectionReason::Locked { .. }
			}
		));
	}

	#[test]
	fn a_present_worktree_is_complete_advanced_or_diverged_by_start_relation() {
		let start = oid("0123456789abcdef0123456789abcdef01234567");
		let current = oid("89abcdef0123456789abcdef0123456789abcdef");
		// A registered, consistent worktree at `current`, with the given ancestry relation to `start`.
		let present = |relation: Option<StartRelation>| WorktreeInspection {
			registration: Registration::Present {
				admin_dir: PathBuf::from("/repo/.git/worktrees/wt"),
			},
			cross_pointers: CrossPointerHealth::Consistent,
			head: Some(HeadFacts {
				state: HeadKind::Symbolic,
				branch: Some("refs/heads/feature".to_owned()),
				object: Some(current.clone()),
			}),
			destination_kind: DestinationKind::LinkedWorktreeCheckout,
			start: Some(start.clone()),
			start_relation: relation,
			..base()
		};

		// Exact object → complete; ancestor → advanced; diverged → a conflict, never "advanced".
		assert!(matches!(
			classify(&present(Some(StartRelation::Equal))),
			WorktreeClassification::CompleteIdempotent { .. }
		));
		assert!(matches!(
			classify(&present(Some(StartRelation::Ancestor))),
			WorktreeClassification::MatchingAdvanced { .. }
		));
		assert!(matches!(
			classify(&present(Some(StartRelation::Diverged))),
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::BranchAtUnexpectedObject { .. }
			}
		));
		// No start expectation → complete (idempotent present worktree).
		assert!(matches!(
			classify(&WorktreeInspection {
				start: None,
				start_relation: None,
				..present(None)
			}),
			WorktreeClassification::CompleteIdempotent { .. }
		));
	}
}
