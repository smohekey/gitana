//! Safe, force-free removal of a linked worktree, reconciling against the read-only inspection so a repeat
//! is idempotent and every unsafe state is refused rather than acted on.
//!
//! `remove` **inspects first** (with a working-tree status, so dirtiness is seen), decides the removal
//! action, then — **immediately before any destructive effect** — re-inspects and re-decides, so a lost race
//! is reported as a conflict rather than overwriting the winner. It refuses a dirty/conflicted, locked,
//! primary, or identity-mismatched worktree; it never deletes a branch or its commits; and it preserves
//! unrelated content and administration. A registration whose checkout is gone is a *recoverable partial*:
//! when the destination is absent or an **empty** directory, the retained admin (and the empty leftover) is
//! dropped so a retry can proceed; a *non-empty* directory at the recorded path is refused and preserved,
//! since no signal proves its current contents are this worktree's own (git's own `worktree remove` refuses a
//! prunable-with-directory the same way). A live checkout is removed only when its working tree contains
//! **solely tracked files** — a matcher-independent index scan, so a non-git-faithful `.gitignore` match can
//! never authorise deleting a non-tracked file. Any residual untracked *or ignored* content is preserved (a
//! deliberately conservative divergence from `git worktree remove`, which deletes ignored build artifacts):
//! the caller clears it first.

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use std::path::Path;

	use crate::admin_cleanup::{deregister_admin, path_absent};
	use crate::facts::{HeadKind, LockState};
	use crate::head::{read_lock_reason, structural_head_branch};
	use crate::inspect::{
		CrossPointerHealth, DestinationKind, IdentityConflict, Registration, WorktreeInspection,
		inspect,
	};
	use crate::pointers::{
		RefSource, SYMREF_MAXDEPTH, admin_dirs_for, admin_gitdir_target, canonical_eq,
		checkout_gitfile_names, is_bare, is_leaf_symlink, main_checkout_identifies_common,
		resolve_ref_terminal,
	};
	use crate::query::WorktreeQuery;
	use crate::registration_lock::RegistrationLock;
	use crate::remove_error::RemoveError;
	use crate::remove_outcome::RemoveOutcome;
	use crate::remove_request::RemoveRequest;
	use crate::repo_id::{detect_kind, open_store_raw, reject_unsupported_repository_format};
	use crate::{LinkedWorktreeError, ProtectionReason, RemovePolicy, WorktreeClassification};

	/// What a removal should do, decided from the inspection.
	#[derive(Debug, Clone, PartialEq, Eq)]
	enum RemoveAction {
		/// The exact worktree is already absent — a no-op.
		AlreadyAbsent,
		/// A live, consistent, clean checkout of this repository — remove its checkout directory and admin.
		RemoveFull {
			/// The admin directory `<common>/worktrees/<name>`.
			admin: std::path::PathBuf,
		},
		/// A recoverable partial with an absent-or-empty destination — drop the retained admin (and the empty
		/// leftover directory, if any). `decide_remove` only produces this for an empty/absent destination; a
		/// non-empty one is refused as a `DestinationConflict`, never cleaned.
		CleanPartial {
			/// The admin directory whose checkout is gone.
			admin: std::path::PathBuf,
		},
	}

	/// Remove the linked worktree described by `request`, dispatching on its [`RemovePolicy`].
	///
	/// [`Conservative`](RemovePolicy::Conservative) — and `GitCompat { force: 0 }` — take the safe, force-free
	/// [`remove_conservative`] path; [`GitCompat`](RemovePolicy::GitCompat) with `force >= 1` takes the
	/// git-faithful forced [`remove_git_forced`] path (git's `worktree remove -f` / `-f -f`).
	///
	/// Returns [`RemoveOutcome::Removed`] on success (retaining the branch and its commits) or
	/// [`RemoveOutcome::AlreadyAbsent`] when the exact worktree is already gone (idempotent). Every
	/// refusal/failure is a [`RemoveError`].
	pub async fn remove(request: &RemoveRequest) -> Result<RemoveOutcome, RemoveError> {
		match request.policy {
			RemovePolicy::GitCompat { force } if force >= 1 => remove_git_forced(request, force).await,
			// `Conservative` or `GitCompat { force: 0 }` — the safe, force-free path.
			_ => remove_conservative(request).await,
		}
	}

	/// The safe, force-free removal (the [`Conservative`](RemovePolicy::Conservative) path, and
	/// `GitCompat { force: 0 }`) — reconciling against the read-only inspection so a repeat is idempotent and
	/// every unsafe state is refused rather than acted on.
	///
	/// Returns [`RemoveOutcome::Removed`] on success (retaining the branch and its commits) or
	/// [`RemoveOutcome::AlreadyAbsent`] when the exact worktree is already gone (idempotent). Every
	/// refusal/failure is a [`RemoveError`].
	async fn remove_conservative(request: &RemoveRequest) -> Result<RemoveOutcome, RemoveError> {
		if !request.destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.destination.clone()).into());
		}
		let common = request.repo.common_dir();
		let query = remove_query(request, true);

		// `GitCompat { force: 0 }` gets git force-0 parity on the residual gate: an *ignored-only* worktree is
		// removable (git deletes it), while a real dirty/untracked one still refuses. `Conservative` passes
		// `false`, so its decision is unchanged. Only the ignored-only residual axis is relaxed.
		let relax = matches!(request.policy, RemovePolicy::GitCompat { force: 0 });

		// A static path fact (unchanged across the re-check): whether the destination *encloses* the
		// repository's own git storage (its common dir lives inside the checkout — a supported
		// `--separate-git-dir`/relocated-bare topology). Recursively deleting such a checkout would destroy the
		// repo's refs and objects, so `decide_remove` refuses it outright, ahead of any content check.
		let enclosed = common_dir_within(&request.destination, common);

		// Serialize registration mutations for the repository so a lost race is a **conflict, not an
		// overwrite**: hold the per-repository lock across the whole decision→re-verify→destroy section, so a
		// concurrent create/repair/re-registration cannot slip in during the residual TOCTOU window between
		// the pre-destroy re-inspect and the delete. Released on any return (and on cancellation).
		let _lock = RegistrationLock::acquire(common).await?;

		// Lock-first even under *corrupted administration*: read the lock file **directly** — no HEAD/index
		// parse — before any inspection, so a locked worktree with a malformed `HEAD` (or a broken referenced
		// ref) still returns the structured `Locked` refusal rather than a `Failed` from resolving HEAD, as
		// stock git reports the lock first. A valid registration resolves to at most one admin; a
		// duplicate/foreign one falls through to the full inspection (which surfaces it as an identity conflict).
		if let [admin] = admin_dirs_for(common, &request.destination)?.as_slice()
			&& let LockState::Locked { reason } = read_lock_reason(admin)
		{
			return Err(RemoveError::Refused(
				WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { reason },
				},
			));
		}

		// Protection preflight **without a status walk**: a primary / enclosure / identity / conflict refusal
		// does not need to read the (possibly unreadable) index or working tree. So a locked worktree with a
		// malformed index still returns its structured refusal — git's lock-first behaviour — rather than a
		// `Failed` from the status computation. Only when the preflight would proceed to a destructive action
		// do we run the status-bearing inspection that the dirty/residual gate needs.
		let preflight = inspect(&remove_query(request, false)).await?;
		match decide_remove(
			&preflight,
			is_primary_worktree(&preflight, common)?,
			enclosed.as_deref(),
			relax,
		) {
			Ok(RemoveAction::AlreadyAbsent) => {
				return Ok(RemoveOutcome::AlreadyAbsent {
					destination: request.destination.clone(),
				});
			}
			Ok(RemoveAction::RemoveFull { .. } | RemoveAction::CleanPartial { .. }) => {}
			Err(e) => return Err(e),
		}

		let inspection = inspect(&query).await?;
		let action = decide_remove(
			&inspection,
			is_primary_worktree(&inspection, common)?,
			enclosed.as_deref(),
			relax,
		)?;

		let admin = match &action {
			RemoveAction::AlreadyAbsent => {
				return Ok(RemoveOutcome::AlreadyAbsent {
					destination: request.destination.clone(),
				});
			}
			RemoveAction::RemoveFull { admin } | RemoveAction::CleanPartial { admin } => admin.clone(),
		};

		// Re-verify identity *immediately before* the destructive effect (the requirement's mandatory
		// re-check). A race that changed the state aborts without deleting: if it is now refused (dirty, locked,
		// a re-appeared conflict) surface that refusal; if it is already gone report the idempotent no-op; if it
		// became a *different* removable shape do not delete a moved target — report it for re-inspection.
		let recheck = inspect(&query).await?;
		match decide_remove(
			&recheck,
			is_primary_worktree(&recheck, common)?,
			enclosed.as_deref(),
			relax,
		) {
			Ok(action_again) if action_again == action => {}
			Ok(RemoveAction::AlreadyAbsent) => {
				return Ok(RemoveOutcome::AlreadyAbsent {
					destination: request.destination.clone(),
				});
			}
			Ok(_) => return Err(RemoveError::Incomplete(Box::new(recheck))),
			Err(e) => return Err(e),
		}

		// Report the branch from the *accepted recheck* — the state removal actually acted on — so a concurrent
		// branch switch between the two inspections is not misreported from the stale first look. Only a
		// **born, shared** branch (`HeadKind::Symbolic` resolving to `refs/heads/*`) is a retained ref: an
		// unborn orphan HEAD names a ref that does not exist yet, and a per-worktree ref (`refs/worktree/*`,
		// `refs/bisect/*`, `refs/rewritten/*`) lives *inside* the admin dir removed here — neither survives, so
		// neither is reported as retained (matching the outcome's contract).
		let retained_branch = recheck
			.head
			.as_ref()
			.filter(|h| h.state == HeadKind::Symbolic)
			.and_then(|h| h.branch.clone())
			.filter(|b| b.starts_with("refs/heads/"));
		perform_remove(request, &action, &admin).await?;
		Ok(RemoveOutcome::Removed {
			destination: request.destination.clone(),
			retained_branch,
		})
	}

	/// The read-only query mirroring this removal request's identity — carrying `expected_branch` (so a
	/// worktree on a *different* branch surfaces as an identity conflict). `with_status` requests the
	/// working-tree status + residual scan (needed by the dirty/residual gate); the lock-first protection
	/// preflight passes `false` so a protection refusal never depends on reading a possibly-unreadable index.
	fn remove_query(request: &RemoveRequest, with_status: bool) -> WorktreeQuery {
		WorktreeQuery {
			repo: request.repo.clone(),
			destination: request.destination.clone(),
			expected_branch: request.expected_branch.clone(),
			start: None,
			with_status,
		}
	}

	/// Whether the inspected destination is the repository's **primary/main** worktree — never removed by this
	/// safe surface. This is judged **only** by the destination's own `.git` currently identifying the shared
	/// common dir (an ordinary primary's `.git` *is* `common`; a `--separate-git-dir` primary's `.git` is a
	/// gitfile naming `common`) — deliberately **independent of registration state**. A stale or malformed
	/// admin entry can register the *primary's* path as a linked worktree (inspection then reports it
	/// `PresentCheckoutMissing`); trusting that registration would let removal delete the primary checkout, so
	/// primary identity is established from the checkout itself and takes precedence over any registration. A
	/// genuine linked worktree's `.git` names its admin (not `common`), so this is `false` for it. A bare
	/// repository has no primary worktree.
	fn is_primary_worktree(
		inspection: &WorktreeInspection,
		common: &Path,
	) -> Result<bool, LinkedWorktreeError> {
		// Never resolve a symlinked destination to judge primary-ness — following the alias to a real `.git`
		// would breach the no-follow boundary. A symlink destination is not a worktree; it refuses below as a
		// destination conflict, never as the primary.
		if is_leaf_symlink(&inspection.destination) {
			return Ok(false);
		}
		// A non-directory destination (a file, FIFO, …) is never the primary worktree. Skip probing
		// `<destination>/.git` for it — that stat would fail with `ENOTDIR` and surface as `Failed` instead of
		// the intended `DestinationConflict` refusal the destination kind already implies.
		if inspection.destination_kind == DestinationKind::OtherFsObject {
			return Ok(false);
		}
		Ok(!is_bare(common)? && main_checkout_identifies_common(&inspection.destination, common)?)
	}

	/// Decide the removal action from the inspection, or refuse. Precedence is most-specific-refusal-first:
	/// primary, then locked, then an identity/integrity conflict, then a recoverable partial, then a live
	/// clean checkout, then a foreign checkout, then already-absent / unrelated content.
	///
	/// `relax_ignored_residual` is set **only** for `GitCompat { force: 0 }` (git force-0 parity): it relaxes
	/// the residual-content gate to tolerate an *ignored-only* worktree (see that gate), matching stock
	/// `git worktree remove` with no `-f`, which deletes an ignored-only checkout. `Conservative` passes
	/// `false`, so its decision is byte-identical to before — every residual (untracked *or* ignored) still
	/// refuses. It relaxes **only** that axis; the sparse/diverged/staged/unreachable-anchor refusals are
	/// unaffected.
	fn decide_remove(
		inspection: &WorktreeInspection,
		is_primary: bool,
		enclosed_common: Option<&Path>,
		relax_ignored_residual: bool,
	) -> Result<RemoveAction, RemoveError> {
		use WorktreeClassification as C;

		// 0. The primary worktree is never removed here (git: "is a main working tree").
		if is_primary {
			return Err(RemoveError::IsPrimaryWorktree(
				inspection.destination.clone(),
			));
		}

		// 0a. The destination encloses the repository's own git storage — recursively deleting it would destroy
		//     the repo. Refused ahead of any lock/identity/content check (the most catastrophic outcome).
		if let Some(common) = enclosed_common {
			return Err(RemoveError::EnclosesRepository(common.to_path_buf()));
		}

		// 1. A locked registration is protected — this surface has no force to override it.
		if let LockState::Locked { reason } = &inspection.lock {
			return Err(RemoveError::Refused(C::ProtectedWithReason {
				reason: ProtectionReason::Locked {
					reason: reason.clone(),
				},
			}));
		}

		// 2. An identity/integrity conflict — a cross-pointer disagreement, a duplicate registration, a foreign
		//    `.git`, or (with a pinned `expected_branch`) a worktree on a different branch — is a mismatch we
		//    refuse rather than remove.
		if let Some(detail) = &inspection.identity_conflict {
			return Err(RemoveError::Refused(C::IdentityConflict {
				detail: detail.clone(),
			}));
		}

		// 3. A recoverable partial: an owned registration whose checkout is gone. Clean it **only when the
		//    destination is absent or an empty directory** — there is then no unknown content to lose, so the
		//    retained admin (and the empty leftover, if any) is removed to unblock a retry. A *non-empty*
		//    directory at the recorded path is **not** verifiably this worktree's own content — it may be a
		//    git-created prunable's leftover, or a path since reused for unrelated data — and no historical
		//    signal (a registration or a marker) proves current ownership, so it is refused and preserved
		//    exactly as git's own `worktree remove` refuses a prunable-with-directory. (git-parity, matching the
		//    empty-only cleanup decision.)
		if let Registration::PresentCheckoutMissing { admin_dir } = &inspection.registration {
			return match inspection.destination_kind {
				DestinationKind::Absent | DestinationKind::EmptyDir => {
					// The admin still holds the per-worktree index. If it has staged/unmerged work, cleaning the
					// partial would erase it (and orphan its index-only blobs), so refuse — as a live checkout's
					// staged changes are a `Dirty` refusal. Mirrors `classify`.
					if inspection.partial_staged_changes == Some(true) {
						return Err(RemoveError::Refused(C::ProtectedWithReason {
							reason: ProtectionReason::StagedContentInMissingCheckout,
						}));
					}
					// Cleaning the partial drops the admin dir (the checkout is gone, but the admin still holds `HEAD`
					// and the per-worktree refs). If any commit anchored only there is reachable from no shared ref,
					// dropping the admin would orphan it, so refuse (naming the commit) exactly as the live path and
					// `classify` do — otherwise "prune and retry" silently discards it.
					if let Some(commit) = &inspection.unreachable_admin_anchor {
						return Err(RemoveError::Refused(C::ProtectedWithReason {
							reason: ProtectionReason::UnreachableAnchoredCommit {
								commit: commit.clone(),
							},
						}));
					}
					Ok(RemoveAction::CleanPartial {
						admin: admin_dir.clone(),
					})
				}
				_ => Err(RemoveError::Refused(C::DestinationConflict {
					kind: inspection.destination_kind.clone(),
				})),
			};
		}

		// 4. A live checkout registered to this repository. When cross-pointer-consistent, a dirty/conflicted
		//    working tree is protected (removing it would discard user work); otherwise it is removed with its
		//    branch retained. (A Present registration with inconsistent cross-pointers is already an identity
		//    conflict above, so the inner guard is defensive.)
		if let Registration::Present { admin_dir } = &inspection.registration
			&& inspection.cross_pointers == CrossPointerHealth::Consistent
		{
			// Tracked-side dirtiness (staged/unstaged/conflicted/missing) is a `Dirty` refusal. Deliberately
			// judged from *tracked* changes only, **not** `is_clean()`: the untracked-path list from
			// `gitana-worktree::status` can false-positive under `core.ignorecase` (a case-only rename) or a
			// non-git-faithful ignore match, and the matcher-independent residual scan below is the authoritative
			// untracked/ignored check — so a status quirk never makes a clean worktree look dirty.
			// A sparse index makes the computed status unreliable (unexpanded `040000` sparse-directory entries
			// produce spurious add/delete pairs), so refuse honestly *before* the status-derived gates rather
			// than delete on — or misreport — that bogus status. Mirrors `classify`. Expanding sparse indexes is
			// a deferred follow-up.
			if inspection.sparse_index == Some(true) {
				return Err(RemoveError::Refused(C::ProtectedWithReason {
					reason: ProtectionReason::SparseIndexUnsupported,
				}));
			}
			if let Some(status) = &inspection.status
				&& status.has_tracked_changes()
			{
				return Err(RemoveError::Refused(C::ProtectedWithReason {
					reason: ProtectionReason::Dirty(Box::new(status.clone())),
				}));
			}
			// A present tracked file whose content hash diverges from the index is a tracked-side edit `status`
			// can miss — a same-size/stat-preserving rewrite or a coarse-timestamp filesystem defeats its stat
			// cache, and skip-worktree entries are omitted entirely. This re-verification hashes every present
			// tracked file, so removing the worktree cannot silently discard such an edit. Mirrors `classify`.
			if let Some(paths) = &inspection.diverged_tracked_content
				&& !paths.is_empty()
			{
				return Err(RemoveError::Refused(C::ProtectedWithReason {
					reason: ProtectionReason::ModifiedTrackedContent {
						paths: paths.clone(),
					},
				}));
			}
			// Conservative residual-content gate: refuse when the working tree holds any file that is not in
			// the index (untracked *or* ignored). This is matcher-independent, so a non-git-faithful
			// `.gitignore` false-positive can never authorise recursively deleting a git-untracked file — the
			// removal preserves it. The offending paths are reported so a caller knows what to clear. (A
			// genuinely untracked file also fails `is_clean` above; this additionally catches the residue an
			// unreliable ignore match would otherwise hide, and reports ignored content the status omits.)
			//
			// **GitCompat{0} exception (git force-0 parity):** stock `git worktree remove` (no `-f`) deletes an
			// *ignored-only* worktree but keeps a real dirty/untracked one. When `relax_ignored_residual` is set
			// (only `GitCompat { force: 0 }`) **and** the working-tree status reports **no untracked path**, every
			// residual file is git-ignored, so we fall through and remove — matching git. `Conservative`
			// (`relax=false`) still refuses. A real untracked file (`status.has_untracked()`) always refuses, at
			// every policy — the matcher-independent scan still guards a non-git-faithful ignore false-positive
			// whenever status sees an untracked path.
			//
			// **Accepted limitation (destructive):** the ignored-only branch trusts this crate's ignore matcher
			// (`gitana-worktree::ignore`) to decide deletion — if that matcher *over-matches* git (classifying a
			// git-*untracked* file as ignored on syntax it does not implement git-faithfully), force-0 removal
			// could delete a file git would preserve. This is deliberate: `GitCompat` is git's CLI surface, and
			// this is exactly the trust the native `gta worktree remove` already placed in the same matcher, so it
			// is git/native parity rather than a new hazard. `Conservative` (Code Henge) never takes this branch,
			// so its matcher-independent guarantee is intact. A git-faithful matcher would remove the caveat.
			if let Some(residual) = &inspection.residual_paths
				&& !residual.is_empty()
			{
				let ignored_only = relax_ignored_residual
					&& inspection
						.status
						.as_ref()
						.is_some_and(|s| !s.has_untracked());
				if !ignored_only {
					return Err(RemoveError::Refused(C::ProtectedWithReason {
						reason: ProtectionReason::ResidualContent {
							paths: residual.clone(),
						},
					}));
				}
			}
			// A clean checkout that anchors a commit only inside its admin dir — a detached/per-worktree-symbolic
			// HEAD, or a `refs/worktree|bisect|rewritten/*` tip — reachable from no surviving shared ref would
			// orphan that commit on removal: refuse, discharging the commit-preservation contract, so the caller
			// can branch/tag it first. Mirrors `classify`. A HEAD anchored by a surviving `refs/heads/*` branch is
			// not at risk and never reported here.
			if let Some(commit) = &inspection.unreachable_admin_anchor {
				return Err(RemoveError::Refused(C::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit {
						commit: commit.clone(),
					},
				}));
			}
			return Ok(RemoveAction::RemoveFull {
				admin: admin_dir.clone(),
			});
		}

		// 5. A checkout present at the destination but *not registered to this repository* (or inconsistent) is
		//    not ours to remove.
		if inspection.destination_kind == DestinationKind::LinkedWorktreeCheckout {
			return Err(RemoveError::Refused(C::PartialConflicting {
				detail: inspection.cross_pointers.clone(),
			}));
		}

		// 6. Nothing of ours is here: an absent/empty destination (or a never-completed interrupted branch with
		//    no registration) is already-absent — idempotent, and the branch is retained.
		if matches!(
			inspection.destination_kind,
			DestinationKind::Absent | DestinationKind::EmptyDir
		) {
			return Ok(RemoveAction::AlreadyAbsent);
		}

		// 7. Unrelated content with no registration — never deleted (it is not a worktree of this repository).
		Err(RemoveError::Refused(C::DestinationConflict {
			kind: inspection.destination_kind.clone(),
		}))
	}

	/// Delete the worktree's checkout (or empty leftover) **then** its admin directory — git's ordering, and
	/// strictly so: the admin is dropped **only after the checkout is confirmed gone**. If checkout deletion
	/// fails (permissions, an open handle, an unremovable spelling) the registration is left intact — a
	/// *registered, repairable* worktree — rather than an orphaned checkout whose `.git` points at a
	/// now-missing admin (which a retry would misread as an identity conflict, losing per-worktree metadata).
	/// Success requires both the checkout and the exact admin path to be absent; anything short is a
	/// re-inspectable [`RemoveError::Incomplete`].
	async fn perform_remove(
		request: &RemoveRequest,
		action: &RemoveAction,
		admin: &Path,
	) -> Result<(), RemoveError> {
		// Defence-in-depth against deleting *through* a leaf symlink: a destination that is itself a symlink —
		// including the `.../wt-link/` trailing-separator spelling that hides one from a naive stat — must never
		// be canonicalized-then-deleted, or `remove_dir_all` would destroy the symlink's *target* (the real
		// worktree) and leave a dangling link, falsely reporting `Removed`. Inspection already classifies such a
		// destination as `OtherFsObject` (refused), so this only guards a symlink swapped in after that check.
		if is_leaf_symlink(&request.destination) {
			let post = inspect(&remove_query(request, false)).await?;
			return Err(RemoveError::Incomplete(Box::new(post)));
		}
		// Resolve the destination to its real path **before** deleting anything. A caller may pass a valid but
		// non-normalized alias (`.../wt/.`, `.../wt/sub/..`) or one through a symlinked parent; the raw spelling
		// is unsafe for the destructive primitives here. `remove_dir_all(".../wt/sub/..")` empties `wt` but
		// leaves the directory (the OS cannot `rmdir` a `..` leaf), and `path_absent(".../wt/sub/..")` then reads
		// the now-dangling alias as `NotFound` — a **false `Removed`** over a directory that still exists.
		// `canonical` resolves via the OS (following symlinks the real way, so a `..` pops the *resolved*
		// location) and tolerates an already-absent leaf, so both the deletion and the post-check act on the
		// actual target. Identity/enclosure/status were judged by inspection, which compares canonically already.
		let destination = crate::pointers::canonical(&request.destination);
		match action {
			RemoveAction::RemoveFull { .. } => {
				// A live, pristine checkout (only tracked files — verified before this) — remove all of it.
				let _ = std::fs::remove_dir_all(&destination);
			}
			RemoveAction::CleanPartial { .. } => {
				// Empty-only: `remove_dir` fails (leaving it) on a raced-in non-empty leftover, never recursing.
				let _ = std::fs::remove_dir(&destination);
			}
			RemoveAction::AlreadyAbsent => unreachable!("AlreadyAbsent is handled before perform_remove"),
		}

		// Drop the admin **only once the checkout is confirmed gone** — a still-present checkout keeps its
		// registration (repairable), never an orphaned checkout pointing at a deleted admin.
		if !path_absent(&destination) {
			let post = inspect(&remove_query(request, false)).await?;
			return Err(RemoveError::Incomplete(Box::new(post)));
		}

		// De-register the admin **atomically**, by renaming it out of `worktrees/` before deleting its bytes.
		// A `rename` moves the whole directory in one step regardless of its children, so an *undeletable* child
		// (an immutable/`chflags`-locked file, which no tool — git included — can unlink) can never leave a
		// *recognisable* half-deleted registration: the moment the rename lands, `admin_dirs_for` no longer sees
		// a registration under `worktrees/`, and any lingering bytes are harmless cruft *outside* it. Only a
		// failure to even rename (e.g. an unwritable `worktrees/`) leaves the registration in place — reported
		// as a re-inspectable `Incomplete` that a retry recognises. (Deleting the identity files in place cannot
		// give this guarantee: recognition needs both `gitdir` and `commondir`, so any order risks unlinking one
		// and then failing on the other.)
		match deregister_admin(admin) {
			Ok(()) => Ok(()),
			Err(_) => {
				let post = inspect(&remove_query(request, false)).await?;
				Err(RemoveError::Incomplete(Box::new(post)))
			}
		}
	}

	/// The repository's `common` dir if it lies *inside* `destination` (equal to it, or a descendant) — the
	/// case in which recursively deleting the checkout would also destroy the repository. Walks `common`'s
	/// ancestors and compares each to `destination` by **filesystem identity** (`canonical_eq` — inode-based
	/// where both exist), so a case-insensitive alias (`<base>/DEST` vs a recorded `<base>/dest/...`) is caught
	/// too, not just an exact-string prefix. `None` when the common dir is safely outside the checkout (the
	/// norm).
	fn common_dir_within(destination: &Path, common: &Path) -> Option<std::path::PathBuf> {
		let common_real = crate::pointers::canonical(common);
		let mut ancestor: &Path = &common_real;
		loop {
			if crate::pointers::canonical_eq(ancestor, destination) {
				return Some(common_real.clone());
			}
			ancestor = ancestor.parent()?;
		}
	}

	// ---------------------------------------------------------------------------
	// The git-faithful FORCE path — git's `worktree remove -f` / `-f -f`.
	//
	// `git worktree remove -f` skips the *cleanliness* check (it deletes a dirty/untracked/ignored checkout)
	// but still validates `.git` **structure**; a second `-f` additionally removes a *locked* worktree.
	// Identity, primary, and enclosure are never overridden. This path is a LEAN structural inspection
	// composing the pointer primitives directly — it never calls `inspect()` (the rich, status-bearing path)
	// and never opens the object store or reads an index, matching git, which under force validates the `.git`
	// structure but not the working-tree content.
	// ---------------------------------------------------------------------------

	/// The owned registration for the destination, or the absence/duplication of one.
	enum ForcedAdmin {
		/// No registration under `<common>/worktrees/*` names this destination.
		None,
		/// Exactly one owned registration names this destination.
		Unique(std::path::PathBuf),
		/// More than one registration names it — corruption, never force-removed (an identity conflict).
		Duplicate(Vec<std::path::PathBuf>),
	}

	/// What sits at the destination, by a no-follow stat that **never opens the directory** (unlike
	/// [`classify_destination`](crate::inspect), which reads the first entry to tell empty from non-empty — a
	/// distinction the forced path does not need, since a *present* directory must carry a valid `.git` either
	/// way).
	#[derive(Clone, Copy, PartialEq, Eq)]
	enum ForcedDest {
		/// Nothing exists at the path.
		Absent,
		/// A directory (empty or not — a present checkout must carry a valid `.git` regardless).
		Directory,
		/// A file, symlink, or other non-directory — never a worktree.
		Other,
	}

	/// The lean structural facts a forced removal decides from — composed from the pointer primitives directly;
	/// no rich inspection, no object store, no index read.
	struct GitForcedFacts {
		/// The destination (echoed for the refusal/outcome paths).
		destination: std::path::PathBuf,
		/// The registration resolution for the destination.
		admin: ForcedAdmin,
		/// What sits at the destination on disk (no-follow).
		dest: ForcedDest,
		/// Whether a *present directory*'s `.git` is a regular-file gitfile naming the unique admin — git's
		/// structural `.git`↔admin validity. `false` for a non-directory, no unique admin, or a broken pointer.
		structural_valid: bool,
		/// Whether the unique admin's `HEAD` **exists and is a file** — git's real forced-remove HEAD gate (probed,
		/// git 2.50.1), which validates HEAD *existence*, not content. `std::fs::metadata` follows symlinks, so a
		/// legacy symlink HEAD resolving to a ref file is a valid file; a missing/dangling/directory HEAD is not.
		/// `false` without a unique admin. Required for a *present-directory* forced delete; an **absent**
		/// destination is cleaned regardless (there is no checkout to validate).
		admin_head_valid: bool,
		/// The unique admin's lock state (`Unlocked` without a unique admin).
		lock: LockState,
		/// Whether the destination is the repository's primary/main worktree (never removed, at any force).
		is_primary: bool,
		/// The repository's common dir if it lies *inside* the destination (deleting it would destroy the repo).
		enclosed: Option<std::path::PathBuf>,
		/// The unique admin's HEAD **terminal** branch ref (`refs/...`) — the direct HEAD branch resolved
		/// structurally through the symref chain (`refs/heads/alias -> refs/heads/feature`), matching what the
		/// rich `inspect`/conservative path reports. Used for the `expected_branch` identity pin and the
		/// retained-branch report. `None` when detached, structurally invalid, or the chain is corrupt/cyclic
		/// (unresolvable) — an unresolvable pin is then an identity conflict, and retained reporting degrades to
		/// `None`. Resolution reads only ref files (no object store), and never follows a HEAD symlink to an
		/// external file. Structural HEAD *validity* is judged on the **direct** HEAD (see `admin_head_valid`),
		/// independent of this — a valid direct symbolic HEAD whose chain is broken still validates.
		head_terminal: Option<String>,
		/// The caller's pinned branch as a full ref (`refs/heads/<name>`), if any.
		expected_refname: Option<String>,
	}

	/// Whether the shared branch ref `refname` (`refs/heads/...`) **actually exists** — a lightweight,
	/// no-object-store filesystem check: a loose ref file `<common>/<refname>`, or an entry in
	/// `<common>/packed-refs`. Used so an **unborn** branch (HEAD names it, but no ref exists yet) is not
	/// misreported as retained, honouring [`RemoveOutcome`]'s contract. `refname` is a caller-validated
	/// (`is_valid_refname`) `refs/heads/*`, so `<common>/<refname>` cannot escape the repository.
	fn shared_branch_exists(common: &Path, refname: &str) -> bool {
		// A **file** loose ref is the branch. `std::fs::metadata` FOLLOWS symlinks, so a legacy *symlinked* loose
		// ref (resolving to a real ref file) correctly counts as existing, while a *directory* at
		// `<common>/<refname>` — a ref namespace (`refs/heads/foo` is a directory because `refs/heads/foo/bar`
		// exists), so `foo` itself is unborn — does not (`is_file()` is false). A dangling symlink also fails
		// (`metadata` errors), degrading to "not retained" — the conservative bias.
		if std::fs::metadata(common.join(refname))
			.map(|m| m.is_file())
			.unwrap_or(false)
		{
			return true;
		}
		match std::fs::read_to_string(common.join("packed-refs")) {
			Ok(text) => text.lines().any(|line| {
				// `packed-refs`: `<oid> <refname>` per line, `#`-comment header and `^<peel>` lines skipped.
				let line = line.trim_start();
				!(line.starts_with('#') || line.starts_with('^'))
					&& line.split_once(' ').map(|(_, name)| name.trim()) == Some(refname)
			}),
			Err(_) => false,
		}
	}

	/// A lean structural inspection for the forced path — composes the pointer primitives, never calling
	/// `inspect()` and never opening the object store. Classifies the destination with a no-follow stat and
	/// resolves the owned registration, its structural validity, admin-`HEAD` existence, lock, and direct HEAD.
	fn inspect_git_forced(request: &RemoveRequest) -> Result<GitForcedFacts, RemoveError> {
		let common = request.repo.common_dir();
		let destination = &request.destination;

		// Destination kind — no-follow, and *without* opening the directory. A trailing-separator leaf symlink
		// (`.../wt-link/`) is stripped via `components` so it is seen (POSIX would otherwise follow it), matching
		// the no-follow boundary elsewhere.
		let leaf = destination.components().as_path();
		let dest = match std::fs::symlink_metadata(leaf) {
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => ForcedDest::Absent,
			Err(e) => return Err(LinkedWorktreeError::io("stat destination", destination, e).into()),
			Ok(meta) if meta.is_dir() => ForcedDest::Directory,
			Ok(_) => ForcedDest::Other,
		};

		// The owned registration(s) naming this destination (already `commondir`-owned and gitdir-back-pointing).
		let admin = match admin_dirs_for(common, destination)?.as_slice() {
			[] => ForcedAdmin::None,
			[only] => ForcedAdmin::Unique(only.clone()),
			many => ForcedAdmin::Duplicate(many.to_vec()),
		};

		// Structural `.git`↔admin validity, admin-`HEAD` existence, lock, and the HEAD terminal branch — only a
		// unique registration has these. `.git`-validity requires **both** cross-pointer directions, mirroring the
		// rich `inspect`'s `CrossPointerHealth::Consistent`: the checkout's `.git` names the admin
		// (`checkout_gitfile_names`), **and** the admin's `gitdir` back-pointer resolves to exactly
		// `<destination>/.git` (`admin_gitdir_target` + `canonical_eq`). `admin_dirs_for` matches only on the
		// gitdir target's *parent*, so an admin whose `gitdir` names a *different* filename under the destination
		// still resolves here yet is a cross-pointer disagreement git rejects "not a working tree" — the back
		// check catches it.
		//
		// The admin-`HEAD` gate is git's REAL forced-remove behaviour (probed, git 2.50.1): HEAD must simply
		// **exist and be a file** — git does *not* validate its content grammar (an empty/garbage/padded/symref
		// HEAD is still removed). `std::fs::metadata` *follows* symlinks, so a legacy symlink HEAD resolving to a
		// ref file counts as a valid file, while a missing/dangling/directory HEAD does not. `structural_head_branch`
		// is used ONLY to *name* the branch for identity + retained reporting — never as the validity gate — so a
		// garbage/empty HEAD is valid-for-removal and simply yields no branch to retain.
		let (structural_valid, admin_head_valid, lock, head_terminal) = match &admin {
			ForcedAdmin::Unique(a) => {
				let gitfile = destination.join(".git");
				let back_consistent =
					admin_gitdir_target(a).is_some_and(|back| canonical_eq(&back, &gitfile));
				let structural = dest == ForcedDest::Directory
					&& checkout_gitfile_names(destination, a)?
					&& back_consistent;
				let admin_head_valid = std::fs::metadata(a.join("HEAD"))
					.map(|m| m.is_file())
					.unwrap_or(false);
				// Name the branch (identity + retained) from the direct HEAD, resolved to its terminal ref (follow
				// `refs/heads/alias -> feature`), as the conservative path reports. `HEAD` already consumed one hop,
				// so budget `SYMREF_MAXDEPTH - 1`; a corrupt/cyclic chain → `Err` → `None` (graceful degrade). This
				// is independent of `admin_head_valid`: a content-invalid HEAD is still removable, just unnamed.
				let head_terminal = structural_head_branch(a).flatten().and_then(|direct| {
					resolve_ref_terminal(common, a, &direct, RefSource::Head, SYMREF_MAXDEPTH - 1).ok()
				});
				(
					structural,
					admin_head_valid,
					read_lock_reason(a),
					head_terminal,
				)
			}
			ForcedAdmin::None | ForcedAdmin::Duplicate(_) => (false, false, LockState::Unlocked, None),
		};

		// Primary identity — judged from the checkout itself (never a registration): only a present *directory*
		// can be the primary, and never a bare repo. Restricting to a directory also avoids an `ENOTDIR` probe of
		// `<file>/.git` for a non-directory destination.
		let is_primary = dest == ForcedDest::Directory
			&& !is_bare(common)?
			&& main_checkout_identifies_common(destination, common)?;

		let enclosed = common_dir_within(destination, common);

		Ok(GitForcedFacts {
			destination: destination.clone(),
			admin,
			dest,
			structural_valid,
			admin_head_valid,
			lock,
			is_primary,
			enclosed,
			head_terminal,
			expected_refname: request.expected_branch.as_ref().map(|b| b.refname()),
		})
	}

	/// Decide the forced removal action from the lean facts, or refuse. Precedence: the **never-overridden**
	/// gates first (primary, enclosure, then identity — a duplicate registration or an `expected_branch`
	/// mismatch), then the lock (the only second-force gate), then the structural validation of the destination.
	fn decide_git_forced(facts: &GitForcedFacts, force: u8) -> Result<RemoveAction, RemoveError> {
		use WorktreeClassification as C;

		// 0. The primary worktree is never removed — at any force.
		if facts.is_primary {
			return Err(RemoveError::IsPrimaryWorktree(facts.destination.clone()));
		}
		// 0a. The destination encloses the repository's git storage — deleting it would destroy the repo.
		if let Some(common) = &facts.enclosed {
			return Err(RemoveError::EnclosesRepository(common.clone()));
		}
		// 1. Identity is never overridden. A duplicate registration is corruption; a pinned `expected_branch`
		//    that the destination's HEAD does not carry is a mismatch — compared against the **terminal** branch
		//    (following `refs/heads/alias -> feature`), matching the conservative path. A corrupt/cyclic chain
		//    resolves to `None`, so an unresolvable pin is a conflict (never delete the wrong worktree). The CLI
		//    passes `expected_branch: None`, so this is inert there.
		if let ForcedAdmin::Duplicate(admins) = &facts.admin {
			return Err(RemoveError::Refused(C::IdentityConflict {
				detail: IdentityConflict::DuplicateRegistration {
					admins: admins.clone(),
				},
			}));
		}
		if facts.expected_refname.is_some()
			&& matches!(facts.admin, ForcedAdmin::Unique(_))
			&& facts.head_terminal != facts.expected_refname
		{
			return Err(RemoveError::Refused(C::IdentityConflict {
				detail: IdentityConflict::RegisteredToDifferentBranch {
					found: facts.head_terminal.clone(),
				},
			}));
		}

		let admin = match &facts.admin {
			ForcedAdmin::Unique(a) => a.clone(),
			// Nothing of ours is registered here: an absent destination is already-absent (idempotent); anything
			// present is not this repository's worktree to force-remove.
			ForcedAdmin::None => {
				return match facts.dest {
					ForcedDest::Absent => Ok(RemoveAction::AlreadyAbsent),
					ForcedDest::Directory => Err(RemoveError::Refused(C::DestinationConflict {
						kind: DestinationKind::UnrelatedContent,
					})),
					ForcedDest::Other => Err(RemoveError::Refused(C::DestinationConflict {
						kind: DestinationKind::OtherFsObject,
					})),
				};
			}
			ForcedAdmin::Duplicate(_) => unreachable!("a duplicate registration is handled above"),
		};

		// 2. A locked worktree needs a second force — the only second-force gate (no masking recursion needed).
		if force < 2
			&& let LockState::Locked { reason } = &facts.lock
		{
			return Err(RemoveError::Refused(C::ProtectedWithReason {
				reason: ProtectionReason::Locked {
					reason: reason.clone(),
				},
			}));
		}

		// 3. Structural validation of the registered destination — force does not skip it.
		match facts.dest {
			// A present checkout must carry a valid `.git` naming the admin **and** an existing (file) admin
			// `HEAD`. A present directory whose `.git` is gone, or whose admin `HEAD` is missing or a directory,
			// is a validation refusal — git's "validation failed" — never a forced delete. An *empty* directory is
			// likewise present, so it too must carry a valid `.git`, and refuses.
			ForcedDest::Directory => {
				if facts.structural_valid && facts.admin_head_valid {
					Ok(RemoveAction::RemoveFull { admin })
				} else {
					Err(RemoveError::Refused(C::DestinationConflict {
						kind: DestinationKind::UnrelatedContent,
					}))
				}
			}
			// A registered checkout whose directory is **gone** is a recoverable partial — drop the stale admin
			// (git's prunable). There is no checkout to validate, so git removes the registration regardless of
			// whether `<admin>/HEAD` still exists; the identity/duplicate/lock guards above already applied.
			ForcedDest::Absent => Ok(RemoveAction::CleanPartial { admin }),
			// A file/symlink now sits at a registered path — not a worktree; never deleted through it.
			ForcedDest::Other => Err(RemoveError::Refused(C::DestinationConflict {
				kind: DestinationKind::OtherFsObject,
			})),
		}
	}

	/// The git-faithful forced removal (`git worktree remove -f` / `-f -f`). Acquires the per-repository
	/// registration lock, decides from the lean structural facts, **re-inspects and re-decides immediately
	/// before the destructive effect** (a lost race is a conflict, not an overwrite), then reuses the shared
	/// [`perform_remove`] destroyer. Never opens the object store or reads an index.
	async fn remove_git_forced(
		request: &RemoveRequest,
		force: u8,
	) -> Result<RemoveOutcome, RemoveError> {
		if !request.destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.destination.clone()).into());
		}
		let common = request.repo.common_dir();

		// Serialize registration mutations for the repository (as the conservative path does), held across the
		// decision → re-verify → destroy section so a concurrent create/repair cannot slip into the TOCTOU window.
		let _lock = RegistrationLock::acquire(common).await?;

		// Validate the repository **format** before any destructive action — the conservative path gets this for
		// free from `inspect` opening the repo, but the lean forced path skips `inspect`, so it must do the same
		// config-only check here (never opening the object store). `detect_kind` is the exact primitive the
		// conservative `inspect` path uses (object format + `repositoryformatversion`), returning the same
		// `UnsupportedObjectFormat` refusal; `reject_unsupported_repository_format` adds git's abort on an unknown
		// `extensions.*`. A repo gitana does not fully understand is never force-mutated (requirements 257-258),
		// matching stock `git worktree remove -f -f`, which aborts too. Nothing is deleted on a format refusal.
		detect_kind(&open_store_raw(common, common)?).await?;
		reject_unsupported_repository_format(common)?;

		// Decide from the current state, failing fast on a refusal before the destructive re-check.
		let action = decide_git_forced(&inspect_git_forced(request)?, force)?;
		if let RemoveAction::AlreadyAbsent = action {
			return Ok(RemoveOutcome::AlreadyAbsent {
				destination: request.destination.clone(),
			});
		}

		// Re-inspect + re-decide **immediately before** the destructive effect and require the **same** action
		// (its admin identity included — `RemoveAction` derives `PartialEq`). A state that changed between the two
		// looks — a destination removed-and-recreated into a *different* removable shape, or a re-appeared
		// conflict — must not be deleted: report the idempotent no-op if it is now gone, else a re-inspectable
		// `Incomplete`, never delete a target that changed. This mirrors `remove_conservative`'s re-verify.
		let recheck = inspect_git_forced(request)?;
		let admin = match decide_git_forced(&recheck, force) {
			Ok(again) if again == action => match &action {
				RemoveAction::RemoveFull { admin } | RemoveAction::CleanPartial { admin } => admin.clone(),
				RemoveAction::AlreadyAbsent => unreachable!("AlreadyAbsent returned above"),
			},
			Ok(RemoveAction::AlreadyAbsent) => {
				return Ok(RemoveOutcome::AlreadyAbsent {
					destination: request.destination.clone(),
				});
			}
			Ok(_) => {
				let post = inspect(&remove_query(request, false)).await?;
				return Err(RemoveError::Incomplete(Box::new(post)));
			}
			Err(e) => return Err(e),
		};

		// The retained branch, from the HEAD **terminal** branch (removal never deletes a ref regardless): only a
		// `refs/heads/*` terminal that **actually exists** as a shared loose/packed ref is retained — an unborn
		// branch (HEAD names it, but no ref exists yet), a corrupt/cyclic chain (terminal `None`), or a detached
		// HEAD is reported as `None`, per `RemoveOutcome`'s contract. A per-worktree ref lives inside the removed
		// admin, so it is never a `refs/heads/*` here.
		let retained_branch = recheck
			.head_terminal
			.as_deref()
			.filter(|b| b.starts_with("refs/heads/") && crate::pointers::is_valid_refname(b))
			.filter(|b| shared_branch_exists(common, b))
			.map(str::to_owned);
		perform_remove(request, &action, &admin).await?;
		Ok(RemoveOutcome::Removed {
			destination: request.destination.clone(),
			retained_branch,
		})
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::remove;
