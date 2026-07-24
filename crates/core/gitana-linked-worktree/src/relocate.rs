//! Safe move of a linked worktree's checkout directory — the structured, in-process form of
//! `git worktree move`, extracted from the `gta` CLI so any library consumer (not just the CLI) can move a
//! worktree without spawning `git`.
//!
//! `relocate` holds the per-repository registration lock, inspects `from`, decides the move is safe (a
//! present, cross-pointer-consistent, non-primary, non-enclosing worktree of this repository — unlocked
//! unless `force >= 2`), checks the destination is free (an empty directory is moved onto; a non-empty one,
//! or one still registered to another worktree without enough `force`, is refused), re-verifies immediately
//! before the rename, then moves the checkout directory and repoints the administration.
//!
//! It **preserves each pointer's representation**: a worktree created with `worktree.useRelativePaths`
//! records *relative* cross-pointers so the tree can be relocated as a unit; the relative/absolute style of
//! each side is captured before the rename and re-emitted after, recomputing a relative checkout pointer for
//! the new depth. An absolute checkout pointer moves with the directory and still names the (unmoved) admin,
//! so it is left untouched. A **dirty** worktree moves intact (a move relocates files, not a cleanliness
//! decision), matching stock git.
//!
//! **Submodules are out of scope here**, exactly as for [`remove`](crate::remove): the library has no
//! submodule model. Moving a worktree that holds an *initialized* submodule would strand the submodule's
//! absorbed git dir, so a caller that cares refuses it before delegating — the `gta` CLI guards this around
//! its `relocate` call.

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use std::path::{Path, PathBuf};

	use crate::facts::LockState;
	use crate::head::read_lock_reason;
	use crate::inspect::{
		CrossPointerHealth, DestinationKind, Registration, WorktreeInspection, inspect,
	};
	use crate::pointers::{
		admin_dirs_for, canonical, canonical_eq, is_bare, is_leaf_symlink,
		main_checkout_identifies_common,
	};
	use crate::query::WorktreeQuery;
	use crate::registration_lock::RegistrationLock;
	use crate::relocate_error::RelocateError;
	use crate::relocate_outcome::RelocateOutcome;
	use crate::relocate_request::RelocateRequest;
	use crate::{LinkedWorktreeError, ProtectionReason, WorktreeClassification, classify};

	/// Move the linked worktree at `request.from` to `request.to`. See the module docs for the safety
	/// contract. Returns [`RelocateOutcome::Relocated`] on success or [`RelocateOutcome::AlreadyAt`] when the
	/// worktree already sits at `to` (idempotent). Every refusal/failure is a [`RelocateError`].
	pub async fn relocate(request: &RelocateRequest) -> Result<RelocateOutcome, RelocateError> {
		if !request.from.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.from.clone()).into());
		}
		if !request.to.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.to.clone()).into());
		}

		let common = request.repo.common_dir();

		// Serialize registration mutations for the repository so a lost race is a **conflict, not an
		// overwrite**: hold the per-repository lock across the whole decide → re-verify → move section.
		// Released on any return (and on cancellation).
		let _lock = RegistrationLock::acquire(common).await?;

		// Validate that `from` is a movable worktree (identity, pinned branch, primary, enclosure, lock)
		// **before** the idempotent short-circuit, so `from == to` only reports a no-op for a genuine,
		// movable worktree — not for an absent, foreign, primary, or (without enough force) locked source.
		let admin = decide_relocate(request, common).await?;

		// Idempotent no-op: the (validated) worktree is already where the request asks. Compare by filesystem
		// identity (`canonical_eq`) so a case-only or symlinked re-spelling of the same directory is equal.
		if canonical_eq(&request.from, &request.to) {
			return Ok(RelocateOutcome::AlreadyAt {
				to: request.to.clone(),
			});
		}

		// The destination must be free: refuse a non-empty occupant, and a stale registration naming `to`
		// unless `force` permits dropping it. Returns the stale admins to drop *after* a successful move.
		let stale = prepare_destination(request, common, &admin)?;

		// Re-verify `from` immediately before the move — a race that changed its registration aborts.
		let recheck_admin = decide_relocate(request, common).await?;
		if recheck_admin != admin {
			let post = inspect(&from_query(request)).await?;
			return Err(RelocateError::Incomplete(Box::new(post)));
		}

		perform_relocate(request, &admin, &stale).await?;
		Ok(RelocateOutcome::Relocated {
			from: request.from.clone(),
			to: request.to.clone(),
		})
	}

	/// A read query for the worktree at `from`, pinned to the request's `expected_branch`. No status walk is
	/// needed — a move is not a cleanliness decision.
	fn from_query(request: &RelocateRequest) -> WorktreeQuery {
		WorktreeQuery {
			repo: request.repo.clone(),
			destination: request.from.clone(),
			expected_branch: request.expected_branch.clone(),
			start: None,
			with_status: false,
		}
	}

	/// Inspect `from` and decide it is a movable worktree, returning its admin directory. Precedence mirrors
	/// git and `remove`: primary, then an enclosed repository, then a lock (overridable with `force >= 2`),
	/// then an identity/branch mismatch, then it must be a present, cross-pointer-consistent worktree.
	async fn decide_relocate(
		request: &RelocateRequest,
		common: &Path,
	) -> Result<PathBuf, RelocateError> {
		let inspection = inspect(&from_query(request)).await?;
		if is_primary_worktree(&inspection, common)? {
			return Err(RelocateError::IsPrimaryWorktree(request.from.clone()));
		}
		// `from` enclosing the repository's own git storage (a relocated-bare / `--separate-git-dir`
		// topology): moving it would relocate the repo, the admin dir, and the held lock. Refused after the
		// primary check (an ordinary primary's common dir is inside it too, but that is the more specific
		// `IsPrimaryWorktree`).
		if let Some(enclosed) = common_dir_within(&request.from, common) {
			return Err(RelocateError::EnclosesRepository(enclosed));
		}
		// A locked source needs two forces to move (git's `move -f -f`); a lower force refuses.
		if let LockState::Locked { reason } = &inspection.lock
			&& request.force < 2
		{
			return Err(RelocateError::Refused(
				WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked {
						reason: reason.clone(),
					},
				},
			));
		}
		// A pinned-branch mismatch (or any cross-pointer identity conflict) is refused.
		if let Some(conflict) = &inspection.identity_conflict {
			return Err(RelocateError::Refused(
				WorktreeClassification::IdentityConflict {
					detail: conflict.clone(),
				},
			));
		}
		// Movable = a present, cross-pointer-consistent linked worktree of this repository. Keyed off the
		// inspection directly (not `classify`, which reports a force-overridden lock as `Locked`). Anything
		// else — absent, prunable, foreign, or unrelated content — is refused with `classify`'s reading.
		match &inspection.registration {
			Registration::Present { admin_dir }
				if inspection.cross_pointers == CrossPointerHealth::Consistent =>
			{
				Ok(admin_dir.clone())
			}
			_ => Err(RelocateError::Refused(classify(&inspection))),
		}
	}

	/// Check the destination is free and return the stale registrations to drop after a successful move.
	///
	/// A non-empty directory (or a file) at `to` is refused as occupied. Any admin registration naming `to`
	/// other than the source's own — a live or prunable/checkout-missing worktree — is refused unless `force`
	/// permits: a locked stale registration needs `force >= 2`, an unlocked one `force >= 1`. (A live worktree
	/// at `to` is caught by the occupancy check first.) With enough force the stale admins are returned so the
	/// caller drops them *after* the rename lands.
	fn prepare_destination(
		request: &RelocateRequest,
		common: &Path,
		source_admin: &Path,
	) -> Result<Vec<PathBuf>, RelocateError> {
		if request.to.exists() && dir_non_empty(&request.to) {
			return Err(RelocateError::DestinationOccupied {
				path: request.to.clone(),
				kind: if request.to.is_dir() {
					DestinationKind::UnrelatedContent
				} else {
					DestinationKind::OtherFsObject
				},
			});
		}

		let stale: Vec<PathBuf> = admin_dirs_for(common, &request.to)?
			.into_iter()
			.filter(|admin| !canonical_eq(admin, source_admin))
			.collect();
		if !stale.is_empty() {
			let any_locked = stale
				.iter()
				.any(|admin| matches!(read_lock_reason(admin), LockState::Locked { .. }));
			let required = if any_locked { 2 } else { 1 };
			if request.force < required {
				return Err(RelocateError::DestinationRegistered {
					path: request.to.clone(),
					admin_dir: stale[0].clone(),
				});
			}
		}
		Ok(stale)
	}

	/// Whether the inspected destination is the repository's **primary/main** worktree — never moved by this
	/// safe surface. Judged only by the destination's own `.git` identifying the shared common dir, and never
	/// by following a symlinked destination (mirrors the same check in `remove`).
	fn is_primary_worktree(
		inspection: &WorktreeInspection,
		common: &Path,
	) -> Result<bool, LinkedWorktreeError> {
		if is_leaf_symlink(&inspection.destination) {
			return Ok(false);
		}
		if inspection.destination_kind == DestinationKind::OtherFsObject {
			return Ok(false);
		}
		Ok(!is_bare(common)? && main_checkout_identifies_common(&inspection.destination, common)?)
	}

	/// The shared common dir found *inside* `from` (an enclosed repository), or `None`. Walks up from the
	/// canonical common dir looking for `from`; mirrors the enclosure check `remove` performs.
	fn common_dir_within(from: &Path, common: &Path) -> Option<PathBuf> {
		let common_real = canonical(common);
		let mut ancestor: &Path = &common_real;
		loop {
			if canonical_eq(ancestor, from) {
				return Some(common_real.clone());
			}
			match ancestor.parent() {
				Some(parent) => ancestor = parent,
				None => return None,
			}
		}
	}

	/// Perform the move: capture pointer styles, clear an empty destination directory, rename the checkout,
	/// drop any stale destination registrations, then repoint the administration — preserving each pointer's
	/// relative/absolute representation. A failure *after* the rename (the checkout has moved but its
	/// administration is not fully repointed) is reported as [`RelocateError::Incomplete`], not a plain
	/// failure; a failure of the rename itself (nothing moved) is an ordinary [`Failed`](RelocateError::Failed).
	async fn perform_relocate(
		request: &RelocateRequest,
		admin: &Path,
		stale: &[PathBuf],
	) -> Result<(), RelocateError> {
		// Capture each pointer's representation while the source is still in place.
		let checkout_relative = gitfile_is_relative(&request.from.join(".git"));
		let admin_relative = admin_gitdir_is_relative(&admin.join("gitdir"));

		// An empty directory at the destination would block `rename`; clear it (already validated empty).
		if request.to.is_dir() {
			std::fs::remove_dir(&request.to)
				.map_err(|e| LinkedWorktreeError::io("clearing empty destination", &request.to, e))?;
		}

		std::fs::rename(&request.from, &request.to)
			.map_err(|e| LinkedWorktreeError::io("moving worktree checkout", &request.to, e))?;

		// Past the rename the checkout has moved: a failure now is a *partial* move — re-inspect `from` and
		// report `Incomplete` (falling back to the underlying error only if even the re-inspection fails).
		match finish_relocate(request, admin, stale, checkout_relative, admin_relative) {
			Ok(()) => Ok(()),
			Err(err) => match inspect(&from_query(request)).await {
				Ok(post) => Err(RelocateError::Incomplete(Box::new(post))),
				Err(_) => Err(RelocateError::Failed(err)),
			},
		}
	}

	/// The post-rename administration fix-up: drop stale destination registrations, repoint the admin's
	/// backlink, and (only when the checkout pointer was relative) recompute the checkout's `.git` for its new
	/// depth. Direct writes — no temp file, so nothing in the moved checkout can be clobbered.
	fn finish_relocate(
		request: &RelocateRequest,
		admin: &Path,
		stale: &[PathBuf],
		checkout_relative: bool,
		admin_relative: bool,
	) -> Result<(), LinkedWorktreeError> {
		// Drop stale destination registrations only *after* the move succeeds, so a failed rename would have
		// left them intact (matching git).
		for other in stale {
			std::fs::remove_dir_all(other).map_err(|e| {
				LinkedWorktreeError::io("removing stale destination registration", other, e)
			})?;
		}

		let gitfile = request.to.join(".git");
		let backlink = admin.join("gitdir");
		std::fs::write(
			&backlink,
			format!("{}\n", pointer(admin, &gitfile, admin_relative)),
		)
		.map_err(|e| LinkedWorktreeError::io("updating admin gitdir", &backlink, e))?;
		// An absolute checkout pointer moved with the directory and still names the (unmoved) admin. A
		// relative one is now wrong at the new depth, so recompute it.
		if checkout_relative {
			std::fs::write(
				&gitfile,
				format!("gitdir: {}\n", pointer(&request.to, admin, true)),
			)
			.map_err(|e| LinkedWorktreeError::io("updating checkout .git", &gitfile, e))?;
		}
		Ok(())
	}

	/// Whether a directory holds any entry (a non-directory path counts as occupied).
	fn dir_non_empty(dir: &Path) -> bool {
		match std::fs::read_dir(dir) {
			Ok(mut entries) => entries.next().is_some(),
			Err(_) => true,
		}
	}

	/// `target` from `from_dir`, relative when `prefer_relative` and a relative form exists, else absolute —
	/// exactly how git writes a pointer (a `worktree.useRelativePaths` worktree keeps relative pointers).
	fn pointer(from_dir: &Path, target: &Path, prefer_relative: bool) -> String {
		if prefer_relative && let Some(relative) = relativize(from_dir, target) {
			return relative;
		}
		canonical(target).display().to_string()
	}

	/// `target` expressed relative to `from_dir` (both resolved first), as git writes a
	/// `worktree.useRelativePaths` pointer. `None` when no relative form exists (no shared component, e.g.
	/// different roots) or either path cannot be resolved, so the caller writes an absolute pointer.
	fn relativize(from_dir: &Path, target: &Path) -> Option<String> {
		let from = from_dir.canonicalize().ok()?;
		let from: Vec<_> = from.components().collect();
		let to = target.canonicalize().ok()?;
		let to: Vec<_> = to.components().collect();
		let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
		if shared == 0 {
			return None;
		}
		let ups = from.len() - shared;
		let mut result = PathBuf::new();
		for _ in 0..ups {
			result.push("..");
		}
		for component in &to[shared..] {
			result.push(component.as_os_str());
		}
		Some(result.display().to_string())
	}

	/// Whether the checkout's `.git` gitfile records a relative pointer (`worktree.useRelativePaths`).
	fn gitfile_is_relative(gitfile: &Path) -> bool {
		std::fs::read_to_string(gitfile)
			.ok()
			.and_then(|content| {
				content
					.lines()
					.next()
					.and_then(|line| line.strip_prefix("gitdir:"))
					.map(|dir| Path::new(dir.trim()).is_relative())
			})
			.unwrap_or(false)
	}

	/// Whether an admin `gitdir` file records a relative pointer.
	fn admin_gitdir_is_relative(gitdir: &Path) -> bool {
		std::fs::read_to_string(gitdir)
			.ok()
			.map(|content| Path::new(content.trim()).is_relative())
			.unwrap_or(false)
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::relocate;
