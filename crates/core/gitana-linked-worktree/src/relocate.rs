//! Safe move of a linked worktree's checkout directory — the structured, in-process form of
//! `git worktree move`, extracted from the `gta` CLI so any library consumer (not just the CLI) can move a
//! worktree without spawning `git`.
//!
//! `relocate` holds the per-repository registration lock, checks the source's lock file directly (so a
//! locked worktree with malformed metadata still refuses cleanly), inspects `from`, decides the move is
//! safe (a present, cross-pointer-consistent, non-primary, non-enclosing worktree of this repository —
//! unlocked unless `force >= 2`), checks the destination is free (an empty directory is moved onto; a
//! non-empty one, or one still registered to another worktree without enough `force`, is refused),
//! re-verifies both sides immediately before the rename, then moves the checkout directory and repoints the
//! administration.
//!
//! It **preserves each pointer's representation**: a worktree created with `worktree.useRelativePaths`
//! records *relative* cross-pointers so the tree can be relocated as a unit; the relative/absolute style of
//! each side is captured before the rename and re-emitted after — a relative checkout pointer recomputed for
//! the new depth, an absolute one rewritten to the canonical admin (so a pointer that reached the admin
//! through the old `from` path does not dangle). Pointers are read and written **byte-clean** (as `create`
//! does), so a non-UTF-8 path round-trips exactly. A **dirty** worktree moves intact (a move relocates files, not a cleanliness
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
		admin_dirs_for, canonical, canonical_eq, ensure_representable_path, is_bare, is_leaf_symlink,
		main_checkout_identifies_common, os_string_from_bytes, path_to_bytes, read_worktree_admins,
		worktree_path_of,
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

		// Lock-first, even under corrupted administration: read the source's lock file **directly** — no
		// HEAD/index parse — so a locked worktree with a malformed `HEAD` still returns the structured `Locked`
		// refusal rather than a `Failed` from `inspect`, exactly as stock git and `remove` report the lock
		// first. `force >= 2` overrides the lock (git's `move -f -f`).
		if request.force < 2
			&& let [admin] = admin_dirs_for(common, &request.from)?.as_slice()
			&& let LockState::Locked { reason } = read_lock_reason(admin)
		{
			return Err(RelocateError::Refused(
				WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { reason },
				},
			));
		}

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

		// Fail fast on an occupied or force-insufficient destination.
		prepare_destination(request, common, &admin)?;

		// Re-verify **both** sides immediately before the move: a race that changed `from`'s registration, or
		// (via a concurrent `lock`) the protection of a stale destination admin, aborts without moving. The
		// destination's stale set is recomputed here so the move acts on the just-verified state.
		let recheck_admin = decide_relocate(request, common).await?;
		if recheck_admin != admin {
			let post = inspect(&from_query(request)).await?;
			return Err(RelocateError::Incomplete(Box::new(post)));
		}
		let stale = prepare_destination(request, common, &admin)?;

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
		// A locked (and readable) source needs two forces to move; the lock-first probe above already caught
		// the malformed-metadata case.
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
		let admin_dir = match &inspection.registration {
			Registration::Present { admin_dir }
				if inspection.cross_pointers == CrossPointerHealth::Consistent =>
			{
				admin_dir
			}
			_ => return Err(RelocateError::Refused(classify(&inspection))),
		};
		// Refuse when the checkout equals or contains its own admin directory — a checkout placed directly at
		// `<common>/worktrees/<id>`. Moving it would relocate the admin out of the repository, so the backlink
		// write would fail at its old path and the registration would be lost. The common-dir enclosure check
		// above does not catch this: the admin dir sits *below* the common dir, so `from` enclosing its admin
		// need not enclose the common dir.
		if contains(&request.from, admin_dir) {
			return Err(RelocateError::EnclosesRepository(canonical(admin_dir)));
		}
		Ok(admin_dir.clone())
	}

	/// Check the destination is free and return the stale registrations to drop after a successful move.
	///
	/// The destination is classified with a **shallow, no-follow** stat ([`classify_destination`], not a
	/// symlink-following `Path::exists` and not a full [`inspect`] that parses checkout/admin metadata): an
	/// absent path or an empty directory is free; anything else — a file, a symlink, a non-empty directory, or
	/// a linked-worktree checkout — is refused as occupied with its accurate kind. Reading only shallow facts
	/// here matters: a non-empty target with a *malformed* `.git`, or an absent target whose stale admin has a
	/// malformed `HEAD`, must still yield the occupied / force outcomes rather than a metadata-parse `Failed`.
	///
	/// Any admin registration naming `to` other than the source's own — a live or prunable/checkout-missing
	/// worktree — is refused unless `force` permits: a locked stale registration needs `force >= 2`, an
	/// unlocked one `force >= 1`. The required force is carried in the error so a caller need not re-probe the
	/// admin's (possibly non-UTF-8 or symlinked) lock file. With enough force the stale admins are returned so
	/// the caller drops them *after* the rename lands.
	fn prepare_destination(
		request: &RelocateRequest,
		common: &Path,
		source_admin: &Path,
	) -> Result<Vec<PathBuf>, RelocateError> {
		let kind = destination_occupancy(&request.to)?;
		if !matches!(kind, DestinationKind::Absent | DestinationKind::EmptyDir) {
			return Err(RelocateError::DestinationOccupied {
				path: request.to.clone(),
				kind,
			});
		}

		// Scan **all** admin registrations git would list, matched by their recorded checkout path (git's own
		// listing criterion), excluding the source's own admin — so leaving a duplicate registration for the
		// moved checkout is impossible. This uses the **fail-closed** `read_worktree_admins` (as create/remove
		// do), which — unlike the enumeration helper — includes a *symlinked* admin leaf, and it does not
		// filter by ownership, so a *foreign* admin (broken/retargeted `commondir`) whose `gitdir` still names
		// `to` is caught too. (`admin_dirs_for`, used for the source's lock probe, applies both filters.)
		let stale: Vec<PathBuf> = read_worktree_admins(common)?
			.into_iter()
			.filter(|admin| !canonical_eq(admin, source_admin))
			.filter(|admin| {
				worktree_path_of(admin).is_ok_and(|recorded| canonical_eq(&recorded, &request.to))
			})
			.collect();
		if !stale.is_empty() {
			let any_locked = stale
				.iter()
				.any(|admin| matches!(read_lock_reason(admin), LockState::Locked { .. }));
			let required_force = if any_locked { 2 } else { 1 };
			if request.force < required_force {
				return Err(RelocateError::DestinationRegistered {
					path: request.to.clone(),
					admin_dir: stale[0].clone(),
					required_force,
				});
			}
		}
		Ok(stale)
	}

	/// A shallow, no-follow occupancy classification of the destination — enough to decide free (absent or an
	/// empty directory) vs occupied, **without parsing a target `.git`**. `classify_destination` would parse
	/// it (to tell a linked-worktree checkout from unrelated content), so a non-empty directory holding a
	/// *malformed* `.git` would surface as a `Failed` rather than the "already exists" occupancy git reports —
	/// this only distinguishes empty from non-empty. A leaf symlink is seen (the trailing separator is
	/// stripped so POSIX does not resolve it) and classified as a non-directory occupant.
	fn destination_occupancy(to: &Path) -> Result<DestinationKind, LinkedWorktreeError> {
		let leaf = to.components().as_path();
		match std::fs::symlink_metadata(leaf) {
			Ok(meta) if meta.is_dir() => {
				let mut entries = std::fs::read_dir(to)
					.map_err(|e| LinkedWorktreeError::io("reading destination", to, e))?;
				if entries.next().is_some() {
					Ok(DestinationKind::UnrelatedContent)
				} else {
					Ok(DestinationKind::EmptyDir)
				}
			}
			// A file, symlink, fifo, … — an occupant that is never replaced.
			Ok(_) => Ok(DestinationKind::OtherFsObject),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DestinationKind::Absent),
			Err(e) => Err(LinkedWorktreeError::io("stat destination", to, e)),
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

	/// The shared common dir found *inside* `from` (an enclosed repository), or `None` — the enclosure check
	/// `remove` performs.
	fn common_dir_within(from: &Path, common: &Path) -> Option<PathBuf> {
		contains(from, common).then(|| canonical(common))
	}

	/// Whether `outer` is equal to, or an ancestor of, `inner` (i.e. `inner` lives inside `outer`) — compared
	/// by filesystem identity, walking up from the canonical `inner`.
	fn contains(outer: &Path, inner: &Path) -> bool {
		let inner_real = canonical(inner);
		let mut ancestor: &Path = &inner_real;
		loop {
			if canonical_eq(ancestor, outer) {
				return true;
			}
			match ancestor.parent() {
				Some(parent) => ancestor = parent,
				None => return false,
			}
		}
	}

	/// Perform the move: capture pointer styles, rename the checkout (a rename replaces an empty destination
	/// directory, so none is pre-removed — a rename failure then leaves the caller's directory intact), drop
	/// any stale destination registrations, then repoint the administration — preserving each pointer's
	/// relative/absolute representation. A failure *after* the rename (the checkout has moved but its
	/// administration is not fully repointed) is reported as [`RelocateError::Incomplete`], not a plain
	/// failure; a failure of the rename itself (nothing moved) is an ordinary [`Failed`](RelocateError::Failed).
	async fn perform_relocate(
		request: &RelocateRequest,
		admin: &Path,
		stale: &[PathBuf],
	) -> Result<(), RelocateError> {
		// Capture each pointer's *written* representation (relative vs absolute) while the source is still in
		// place. Read byte-clean from the **raw** pointer text — not a resolved target — so a relative pointer
		// (possibly with non-UTF-8 bytes) is detected as relative, and its style is preserved by the move.
		let checkout_relative = raw_pointer_is_relative(&request.from.join(".git"), true);
		let admin_relative = raw_pointer_is_relative(&admin.join("gitdir"), false);

		// Before any effect, reject a destination (or admin) the pointer files cannot serialise byte-clean —
		// a non-representable path on a non-Unix platform, where `path_to_bytes` is lossy. A no-op on Unix
		// (pointers are byte-clean there), matching `create`, so the move never renames-then-corrupts a
		// backlink it cannot faithfully write.
		ensure_representable_path(&request.to)?;
		ensure_representable_path(admin)?;

		// POSIX `rename` replaces an empty destination directory; Windows `rename` cannot. `prepare_destination`
		// validated `to` as absent or empty, so clear a validated-empty directory first for portability — and
		// restore it (best-effort) if the rename then fails, so a failed move never deletes the caller's
		// (empty) directory.
		let dest_was_empty_dir = matches!(
			destination_occupancy(&request.to),
			Ok(DestinationKind::EmptyDir)
		);
		if dest_was_empty_dir {
			std::fs::remove_dir(&request.to)
				.map_err(|e| LinkedWorktreeError::io("clearing empty destination", &request.to, e))?;
		}
		if let Err(e) = std::fs::rename(&request.from, &request.to) {
			if dest_was_empty_dir {
				let _ = std::fs::create_dir(&request.to);
			}
			return Err(LinkedWorktreeError::io("moving worktree checkout", &request.to, e).into());
		}

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
	/// depth. Pointers are written **byte-clean** (as `create` does). Direct writes — no temp file, so nothing
	/// in the moved checkout can be clobbered.
	fn finish_relocate(
		request: &RelocateRequest,
		admin: &Path,
		stale: &[PathBuf],
		checkout_relative: bool,
		admin_relative: bool,
	) -> Result<(), LinkedWorktreeError> {
		for other in stale {
			std::fs::remove_dir_all(other).map_err(|e| {
				LinkedWorktreeError::io("removing stale destination registration", other, e)
			})?;
		}

		let gitfile = request.to.join(".git");
		let backlink = admin.join("gitdir");

		// admin `gitdir` → the checkout's new `.git`, byte-clean, its original relative/absolute style.
		let mut admin_bytes = path_to_bytes(&pointer(admin, &gitfile, admin_relative));
		admin_bytes.push(b'\n');
		std::fs::write(&backlink, admin_bytes)
			.map_err(|e| LinkedWorktreeError::io("updating admin gitdir", &backlink, e))?;

		// Always rewrite the checkout's `.git` to name the (unmoved) admin, preserving its representation: a
		// relative pointer recomputed for the new depth, an absolute one rewritten to the canonical admin.
		// Rewriting even an absolute pointer matters — a valid absolute pointer that reached the admin *through
		// a path inside `from`* (via a symlink or `..`) would dangle once `from` is gone; the canonical admin
		// path never routes through the moved directory.
		let mut checkout_bytes = b"gitdir: ".to_vec();
		checkout_bytes.extend_from_slice(&path_to_bytes(&pointer(
			&request.to,
			admin,
			checkout_relative,
		)));
		checkout_bytes.push(b'\n');
		std::fs::write(&gitfile, checkout_bytes)
			.map_err(|e| LinkedWorktreeError::io("updating checkout .git", &gitfile, e))?;
		Ok(())
	}

	/// `target` from `from_dir`, relative when `prefer_relative` and a relative form exists, else absolute —
	/// exactly how git writes a pointer (a `worktree.useRelativePaths` worktree keeps relative pointers).
	/// Returned as a [`PathBuf`] so it serialises byte-clean (a non-UTF-8 path is never lossily displayed).
	fn pointer(from_dir: &Path, target: &Path, prefer_relative: bool) -> PathBuf {
		if prefer_relative && let Some(relative) = relativize(from_dir, target) {
			return relative;
		}
		canonical(target)
	}

	/// `target` expressed relative to `from_dir` (both resolved first), as git writes a
	/// `worktree.useRelativePaths` pointer. `None` when no relative form exists (no shared component, e.g.
	/// different roots) or either path cannot be resolved, so the caller writes an absolute pointer. The
	/// components are copied byte-for-byte, so a non-UTF-8 path segment is preserved.
	fn relativize(from_dir: &Path, target: &Path) -> Option<PathBuf> {
		let from = from_dir.canonicalize().ok()?;
		let from: Vec<_> = from.components().collect();
		let to = target.canonicalize().ok()?;
		let to: Vec<_> = to.components().collect();
		let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
		if shared == 0 {
			return None;
		}
		let mut result = PathBuf::new();
		for _ in 0..(from.len() - shared) {
			result.push("..");
		}
		for component in &to[shared..] {
			result.push(component.as_os_str());
		}
		Some(result)
	}

	/// Whether a pointer file records a **relative** path, read byte-clean from its raw first line (a
	/// `.git` gitfile with `strip_gitdir_prefix` true carries a `gitdir: ` prefix; an admin `gitdir` file
	/// carries the bare path). The raw path is inspected without resolving it, so a relative pointer is seen
	/// as relative regardless of where it points, and a non-UTF-8 byte in it does not make the read fail (and
	/// wrongly report absolute). A missing/empty/malformed pointer reads as not-relative (absolute default).
	fn raw_pointer_is_relative(path: &Path, strip_gitdir_prefix: bool) -> bool {
		let Ok(bytes) = std::fs::read(path) else {
			return false;
		};
		let line = bytes.split(|&byte| byte == b'\n').next().unwrap_or(&[]);
		let raw = if strip_gitdir_prefix {
			match line.strip_prefix(b"gitdir:") {
				Some(rest) => rest,
				None => return false,
			}
		} else {
			line
		}
		.trim_ascii();
		!raw.is_empty() && Path::new(&os_string_from_bytes(raw)).is_relative()
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::relocate;
