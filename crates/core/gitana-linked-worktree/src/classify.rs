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
	// A `Dirty(WorktreeStatusReport)` variant is added with the removal slice (it needs a status run).
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
	/// The destination is protected (locked; dirty is added with the removal slice).
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
	let checked_out_elsewhere = match &inspection.requested_branch {
		RequestedBranch::Exists {
			checked_out_elsewhere,
			..
		}
		| RequestedBranch::Absent {
			checked_out_elsewhere,
		} => checked_out_elsewhere.as_ref(),
		RequestedBranch::NotRequested => None,
	};
	if let Some(other) = checked_out_elsewhere {
		return C::BranchUseConflict {
			other_checkout: other.clone(),
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
