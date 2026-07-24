//! Safe move of a linked worktree's checkout directory — `git worktree move` re-expressed as a structured,
//! force-free operation that reconciles against the read-only inspection.
//!
//! `relocate` inspects `from` and `to`, decides the move is safe (a present, cross-pointer-consistent,
//! unlocked, non-primary worktree of this repository, moving to an unoccupied path), then — **immediately
//! before the move** — re-inspects and re-decides, so a lost race is reported rather than overwriting the
//! winner. It refuses a locked, primary, identity-mismatched, missing, or foreign `from`, and an occupied
//! `to`; it never deletes a branch, touches working-tree content, or follows a symlinked destination.
//!
//! The move preserves the worktree's identity. Only the admin's `gitdir` back-pointer is rewritten to the
//! checkout's new `.git`; the admin directory itself (and thus the `git worktree` id git assigned at
//! creation), the branch, and the commits are unchanged. The checkout's own `.git` gitfile records an
//! **absolute** path to the (unmoved) admin dir, so moving the checkout leaves it valid — it is not
//! rewritten. A **dirty** worktree moves intact: a move relocates files, it is not a cleanliness decision,
//! matching stock `git worktree move` (which relocates a dirty worktree; only a *locked* one is refused).

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use std::path::{Path, PathBuf};

	use crate::facts::LockState;
	use crate::head::read_lock_reason;
	use crate::inspect::{DestinationKind, Registration, WorktreeInspection, inspect};
	use crate::pointers::{
		admin_dirs_for, canonical, is_bare, is_leaf_symlink, main_checkout_identifies_common,
		path_to_bytes,
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
		// Idempotent no-op: the worktree is already where the request asks. Compare canonically so a
		// non-normalised or symlinked spelling of the same path is recognised as equal.
		if canonical(&request.from) == canonical(&request.to) {
			return Ok(RelocateOutcome::AlreadyAt {
				to: request.to.clone(),
			});
		}

		let common = request.repo.common_dir();

		// Serialize registration mutations for the repository so a lost race is a **conflict, not an
		// overwrite**: hold the per-repository lock across the whole decide → re-verify → move section, so a
		// concurrent create/remove/relocate cannot slip in during the TOCTOU window between the re-inspect and
		// the rename. Released on any return (and on cancellation).
		let _lock = RegistrationLock::acquire(common).await?;

		// Lock-first, even under corrupted administration: read the lock file **directly** — no HEAD/index
		// parse — so a locked worktree with a malformed `HEAD` still returns the structured `Locked` refusal
		// rather than a `Failed` from resolving HEAD, exactly as stock git reports the lock first.
		if let [admin] = admin_dirs_for(common, &request.from)?.as_slice()
			&& let LockState::Locked { reason } = read_lock_reason(admin)
		{
			return Err(RelocateError::Refused(
				WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { reason },
				},
			));
		}

		// Decide the move is safe (a movable `from`, an unoccupied `to`), then re-verify **immediately before**
		// the move — a race that changed either side aborts without moving.
		let admin = decide_relocate(request, common).await?;
		verify_destination_free(request).await?;

		let recheck_admin = decide_relocate(request, common).await?;
		if recheck_admin != admin {
			// The worktree at `from` became a *different* registration between the decision and the move — do
			// not move a target we no longer identify; report it for re-inspection.
			let post = inspect(&from_query(request)).await?;
			return Err(RelocateError::Incomplete(Box::new(post)));
		}
		verify_destination_free(request).await?;

		perform_relocate(request, &admin)?;
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

	/// Inspect `from` and decide it is a movable worktree, returning its admin directory. Refuses the
	/// primary worktree (git's precedence), then anything the shared [`classify`] reports as not a present,
	/// consistent, live worktree (locked, identity-mismatched, partial, foreign, or absent).
	async fn decide_relocate(
		request: &RelocateRequest,
		common: &Path,
	) -> Result<PathBuf, RelocateError> {
		let inspection = inspect(&from_query(request)).await?;
		if is_primary_worktree(&inspection, common)? {
			return Err(RelocateError::IsPrimaryWorktree(request.from.clone()));
		}
		match classify(&inspection) {
			// A registered, cross-pointer-consistent, present worktree (on a branch, detached, or unborn) —
			// safe to move. Its admin directory is the registration's.
			WorktreeClassification::CompleteIdempotent { .. }
			| WorktreeClassification::MatchingAdvanced { .. }
			| WorktreeClassification::CompletePresent { .. } => match &inspection.registration {
				Registration::Present { admin_dir } => Ok(admin_dir.clone()),
				// A movable classification always has a Present registration; a mismatch means the state
				// changed under us — report it for re-inspection rather than moving on a stale read.
				_ => Err(RelocateError::Incomplete(Box::new(inspection))),
			},
			// Locked, identity conflict, recoverable/foreign partial, or absent — not a movable worktree.
			other => Err(RelocateError::Refused(other)),
		}
	}

	/// Verify nothing occupies `to`. A faithful `git worktree move` refuses any existing target, so only an
	/// absent path is accepted (the caller creates parent directories, not the target itself).
	async fn verify_destination_free(request: &RelocateRequest) -> Result<(), RelocateError> {
		let query = WorktreeQuery {
			repo: request.repo.clone(),
			destination: request.to.clone(),
			expected_branch: None,
			start: None,
			with_status: false,
		};
		let inspection = inspect(&query).await?;
		match inspection.destination_kind {
			DestinationKind::Absent => Ok(()),
			other => Err(RelocateError::DestinationOccupied {
				path: request.to.clone(),
				kind: other,
			}),
		}
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

	/// Perform the move: rename the checkout directory, then rewrite the admin's `gitdir` back-pointer to the
	/// checkout's new `.git`. The rename moves the whole tree — including its `.git` gitfile, which names the
	/// unmoved admin dir with an absolute path and so stays valid — in one step. If the rename lands but the
	/// pointer rewrite fails, the admin still names the old path: a re-inspectable partial (git's `worktree
	/// repair` also fixes it), reported as [`RelocateError::Incomplete`] by the caller on the next inspection.
	fn perform_relocate(request: &RelocateRequest, admin: &Path) -> Result<(), RelocateError> {
		std::fs::rename(&request.from, &request.to)
			.map_err(|e| LinkedWorktreeError::io("moving worktree checkout", &request.to, e))?;

		// Byte-clean, absolute, canonical `<to>/.git` + newline — exactly how `create` writes the admin
		// `gitdir`, so a non-UTF-8 destination round-trips and each side resolves regardless of the cwd.
		let gitfile = canonical(&request.to).join(".git");
		let mut bytes = path_to_bytes(&gitfile);
		bytes.push(b'\n');
		write_file_atomic(&admin.join("gitdir"), &bytes)?;
		Ok(())
	}

	/// Publish `contents` at `path` atomically: write a temp sibling, `fsync` it, then `rename` it onto
	/// `path` (a rename over an existing file is atomic on the same filesystem). Mirrors `create`'s
	/// admin-pointer writes so the `gitdir` is never observed half-written.
	fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<(), LinkedWorktreeError> {
		use std::io::Write as _;

		let mut tmp = path.as_os_str().to_owned();
		tmp.push(format!(".tmp.{}", std::process::id()));
		let tmp = PathBuf::from(tmp);

		let mut file = std::fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.open(&tmp)
			.map_err(|e| LinkedWorktreeError::io("writing admin gitdir", &tmp, e))?;
		file
			.write_all(contents)
			.map_err(|e| LinkedWorktreeError::io("writing admin gitdir", &tmp, e))?;
		file
			.sync_all()
			.map_err(|e| LinkedWorktreeError::io("syncing admin gitdir", &tmp, e))?;
		std::fs::rename(&tmp, path).map_err(|e| {
			let _ = std::fs::remove_file(&tmp);
			LinkedWorktreeError::io("publishing admin gitdir", path, e)
		})
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::relocate;
