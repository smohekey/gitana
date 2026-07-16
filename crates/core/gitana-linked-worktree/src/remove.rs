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

	use crate::facts::{HeadKind, LockState};
	use crate::head::read_lock_reason;
	use crate::inspect::{
		CrossPointerHealth, DestinationKind, Registration, WorktreeInspection, inspect,
	};
	use crate::pointers::{
		admin_dirs_for, is_bare, is_leaf_symlink, main_checkout_identifies_common,
	};
	use crate::query::WorktreeQuery;
	use crate::remove_error::RemoveError;
	use crate::remove_outcome::RemoveOutcome;
	use crate::remove_request::RemoveRequest;
	use crate::{LinkedWorktreeError, ProtectionReason, WorktreeClassification};

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

	/// Remove the linked worktree described by `request`, reconciling against its current state.
	///
	/// Returns [`RemoveOutcome::Removed`] on success (retaining the branch and its commits) or
	/// [`RemoveOutcome::AlreadyAbsent`] when the exact worktree is already gone (idempotent). Every
	/// refusal/failure is a [`RemoveError`].
	pub async fn remove(request: &RemoveRequest) -> Result<RemoveOutcome, RemoveError> {
		if !request.destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.destination.clone()).into());
		}
		let common = request.repo.common_dir();
		let query = remove_query(request, true);

		// A static path fact (unchanged across the re-check): whether the destination *encloses* the
		// repository's own git storage (its common dir lives inside the checkout — a supported
		// `--separate-git-dir`/relocated-bare topology). Recursively deleting such a checkout would destroy the
		// repo's refs and objects, so `decide_remove` refuses it outright, ahead of any content check.
		let enclosed = common_dir_within(&request.destination, common);

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
	fn decide_remove(
		inspection: &WorktreeInspection,
		is_primary: bool,
		enclosed_common: Option<&Path>,
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
			if let Some(residual) = &inspection.residual_paths
				&& !residual.is_empty()
			{
				return Err(RemoveError::Refused(C::ProtectedWithReason {
					reason: ProtectionReason::ResidualContent {
						paths: residual.clone(),
					},
				}));
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

	/// Atomically de-register `admin`: `rename` it to a sibling *outside* `<common>/worktrees/`, then delete the
	/// moved directory best-effort. Returns `Ok` once the registration is gone — either the rename landed (any
	/// undeletable remnant is then harmless cruft under the common dir, not a worktree entry) **or** the admin
	/// was already absent (a concurrent remover/prune finished it). `Err` only when the rename fails with the
	/// admin still in place, leaving the registration recognisable for a retry.
	fn deregister_admin(admin: &Path) -> std::io::Result<()> {
		use std::sync::atomic::{AtomicU64, Ordering};
		static SEQ: AtomicU64 = AtomicU64::new(0);
		// `admin` is `<common>/worktrees/<name>`; move it to a **fixed-length** sibling under the common dir
		// (writable, same filesystem → `rename` is atomic) but *not* under `worktrees/`. The name is bounded
		// (`pid`+`seq`, not the admin's own name) so a near-`NAME_MAX` admin name cannot make the trash name
		// exceed the component limit and fail with `ENAMETOOLONG`.
		let worktrees = admin.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
		let common = worktrees.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
		// `SEQ` makes names unique within this process, but across processes the `pid` can be **reused** after a
		// prior removal crashed leaving a non-empty `.gitana-removing.<pid>.<seq>` remnant — a fresh process
		// restarts `SEQ` at 0 and would target that exact existing dir. `rename` onto a non-empty dir fails
		// (`ENOTEMPTY`), and since the checkout is already deleted by now that would strand the registration as a
		// false `Incomplete`. So advance to the next sequence whenever the chosen trash name already exists,
		// bounded by a (astronomically generous) retry cap.
		const MAX_TRIES: u32 = 4096;
		for _ in 0..MAX_TRIES {
			let seq = SEQ.fetch_add(1, Ordering::Relaxed);
			let trash = common.join(format!(".gitana-removing.{}.{}", std::process::id(), seq));
			// Skip a name that already exists — **before** attempting the rename. POSIX `rename` *replaces* an
			// empty directory at the target, so renaming onto a pre-existing empty `.gitana-removing.*` (a crashed
			// prior run after PID reuse, or an unrelated entry) would clobber it and then delete the replacement.
			// A raced-in target between this check and the rename is caught by the error arm below. (No-replace
			// `renameat2`/`renamex_np` would be atomic, but require `unsafe` libc, which the workspace forbids.)
			if std::fs::symlink_metadata(&trash).is_ok() {
				continue;
			}
			match std::fs::rename(admin, &trash) {
				Ok(()) => {
					let _ = std::fs::remove_dir_all(&trash); // best-effort; a remnant is harmless non-entry cruft
					return Ok(());
				}
				// The **admin** (rename source) is already gone — a concurrent remover/prune de-registered it; the
				// worktree is removed regardless.
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
				// The chosen trash **name** was created between the check above and the rename — try the next
				// sequence rather than fail (or clobber) with the checkout gone.
				Err(_) if std::fs::symlink_metadata(&trash).is_ok() => continue,
				Err(e) => return Err(e),
			}
		}
		Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"deregister_admin: exhausted trash-name sequences",
		))
	}

	/// Whether `path` is *confirmed* absent — a `NotFound` stat (no symlink followed). Any other stat failure
	/// (e.g. a permission error on a parent) is treated as **not** confirmed-absent, so removal reports
	/// `Incomplete` rather than a false success.
	fn path_absent(path: &Path) -> bool {
		matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
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
			match ancestor.parent() {
				Some(parent) => ancestor = parent,
				None => return None,
			}
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::remove;
