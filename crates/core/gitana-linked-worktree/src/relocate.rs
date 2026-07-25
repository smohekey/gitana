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
		CrossPointerHealth, DestinationKind, Registration, WorktreeInspection, inspect_structural_head,
	};
	use crate::pointers::{
		admin_dirs_for, canonical, canonical_eq, ensure_representable_path, is_bare, is_leaf_symlink,
		is_listed_admin, main_checkout_identifies_common, path_from_bytes, path_to_bytes,
		read_worktree_admins, strip_eol_bytes, update_file_in_place, worktree_path_of,
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

		// **Pin both paths once, under the lock, before any validation or effect** — then use the pinned
		// request for every check, the lock probe, the rename, and the pointer writes:
		//   * `from` is canonicalized (it exists). A dot-segment/symlink alias — `/wt/sub/..`, or a leaf
		//     symlink to the checkout, which the removal path and git accept — must be resolved *before* the
		//     lock probe below: `admin_dirs_for` returns no registration for an unresolved alias, so the
		//     lock-first classification would otherwise be skipped and a malformed-checkout source misreport
		//     `Failed` instead of `Locked`. `rename` also rejects a `..`-terminated source with `EINVAL`, so
		//     the rename must act on the resolved path regardless.
		//   * `to` is resolved via `resolve_destination` (its existing parent canonicalized, final component
		//     re-attached), so the occupancy/stale-registration checks and the rename act on **one** symlink-
		//     free pathname rather than re-resolving `request.to` at each step — which alone could let a
		//     retargeted parent point the checks and the rename at different targets. **Deferred hardening
		//     limitation** (like the Windows rename and WTF-8 pointer I/O below): a canonical *pathname* does
		//     not *pin* the parent directory, so a hostile local process that renames `to`'s parent and
		//     installs a symlink in its place, in the instant between the final check and the `rename`, can
		//     still redirect the move to an unchecked target. Fully closing it needs a held parent directory
		//     handle with `renameat`/`openat` (a `rustix` dependency and a departure from this layer's ambient
		//     `std::fs`); code-henge never exposes an attacker-writable destination parent, so it is left as a
		//     documented gap. The pointer writes carry their own no-follow/identity defense (see
		//     `update_file_in_place`).
		// The public outcome still reports the caller's requested paths.
		let normalized = RelocateRequest {
			from: request
				.from
				.canonicalize()
				.unwrap_or_else(|_| request.from.clone()),
			to: resolve_destination(&request.to),
			..request.clone()
		};

		// Lock-first, even under corrupted administration: read the source's lock file **directly** — no
		// HEAD/index parse — so a locked worktree with a malformed `HEAD` still returns the structured `Locked`
		// refusal rather than a `Failed` from `inspect`, exactly as stock git and `remove` report the lock
		// first. Probed on the **resolved** `from` so a supported alias still finds the registration.
		// `force >= 2` overrides the lock (git's `move -f -f`).
		if normalized.force < 2
			&& let [admin] = admin_dirs_for(common, &normalized.from)?.as_slice()
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
		let admin = decide_relocate(&normalized, common).await?;

		// Idempotent no-op: the (validated) worktree is already where the request asks. Compare by filesystem
		// identity (`canonical_eq`) so a case-only or symlinked re-spelling of the same directory is equal.
		if canonical_eq(&normalized.from, &normalized.to) {
			return Ok(RelocateOutcome::AlreadyAt {
				to: request.to.clone(),
			});
		}

		// Fail fast on an occupied or force-insufficient destination.
		prepare_destination(&normalized, common, &admin)?;

		// Re-verify **both** sides immediately before the move: a race that changed `from`'s registration, or
		// (via a concurrent `lock`) the protection of a stale destination admin, aborts without moving. The
		// destination's stale set is recomputed here so the move acts on the just-verified state.
		let recheck_admin = decide_relocate(&normalized, common).await?;
		if recheck_admin != admin {
			let post = inspect_structural_head(&from_query(&normalized)).await?;
			return Err(RelocateError::Incomplete(Box::new(post)));
		}
		let stale = prepare_destination(&normalized, common, &admin)?;

		perform_relocate(&normalized, &admin, &stale).await?;
		Ok(RelocateOutcome::Relocated {
			from: request.from.clone(),
			to: request.to.clone(),
		})
	}

	/// A read query for the worktree at `from`, pinned to the request's `expected_branch`. No status walk is
	/// needed — a move is not a cleanliness decision. Always paired with [`inspect_structural_head`], which
	/// reads HEAD **structurally**: a move validates HEAD's structure and matches its (unpeeled) branch without
	/// following the symref chain, so a worktree whose HEAD chain is cyclic/unreadable is still movable,
	/// exactly as stock git moves it.
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
		let inspection = inspect_structural_head(&from_query(request)).await?;
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
		// Movable = a present, cross-pointer-consistent linked worktree of this repository **with a
		// structurally valid HEAD**. Keyed off the inspection directly (not `classify`, which reports a
		// force-overridden lock as `Locked`). The inspection reads HEAD structurally (via
		// `inspect_structural_head`), so `head.is_some()` here means HEAD is present and well-formed *without*
		// its ref chain being resolved:
		// it matches stock `git worktree move`, which refuses a source whose `<admin>/HEAD` is absent/malformed
		// yet moves one whose HEAD symref chain is merely cyclic or unreadable. Anything else — absent,
		// prunable, HEAD-less, foreign, or unrelated content — is refused with `classify`'s reading.
		let admin_dir = match &inspection.registration {
			Registration::Present { admin_dir }
				if inspection.cross_pointers == CrossPointerHealth::Consistent
					&& inspection.head.is_some() =>
			{
				admin_dir
			}
			// Refuse with `classify`'s reading — but a lock the caller already **overrode** with `force >= 2`
			// (the gate above let validation continue) must not resurface here as `Locked`: a HEAD-less/partial
			// source that happens to be locked would otherwise tell a user who already supplied `-f -f` to
			// supply `-f -f` again, hiding the real defect. Reclassify such a source as if unlocked so it reports
			// the actual invalid state.
			_ => {
				let reading = if request.force >= 2 && matches!(inspection.lock, LockState::Locked { .. }) {
					classify(&WorktreeInspection {
						lock: LockState::Unlocked,
						..inspection.clone()
					})
				} else {
					classify(&inspection)
				};
				return Err(RelocateError::Refused(reading));
			}
		};
		// Refuse when the checkout equals or contains its own admin directory — a checkout placed directly at
		// `<common>/worktrees/<id>`. Moving it would relocate the admin out of the repository, so the backlink
		// write would fail at its old path and the registration would be lost. The common-dir enclosure check
		// above does not catch this: the admin dir sits *below* the common dir, so `from` enclosing its admin
		// need not enclose the common dir.
		if contains(&request.from, admin_dir) {
			return Err(RelocateError::EnclosesRepository(canonical(admin_dir)));
		}

		// Refuse a source whose **pointer files are symlinks**, *before* any rename. Read-side inspection
		// follows a symlinked checkout `.git` / admin `gitdir` (git does), but relocate must rewrite them in
		// place, and following a symlink there would truncate an external target. `update_file_in_place`
		// refuses that at write time — but only *after* the checkout has moved, leaving a partial move whose
		// suggested `gta worktree repair` (a plain follow-the-symlink write) would then clobber the target. So
		// reject them here (checked on both the initial pass and the pre-move re-verification), turning a
		// deterministic partial-move-and-clobber into a clean up-front refusal. A deliberate divergence from
		// git's follow-on-write, matching the crate's no-follow write posture.
		let checkout_gitfile = request.from.join(".git");
		if is_leaf_symlink(&checkout_gitfile) {
			return Err(RelocateError::UntrustedRegistration(checkout_gitfile));
		}
		let admin_gitdir = admin_dir.join("gitdir");
		if is_leaf_symlink(&admin_gitdir) {
			return Err(RelocateError::UntrustedRegistration(admin_gitdir));
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

		// `to` anywhere beneath `<common>/worktrees` is prune-unsafe. `git worktree prune` (and `gta worktree
		// prune`) scan that container and recursively remove any child that is not a live admin — a moved
		// checkout there, a bare child of `worktrees/` or nested inside an unlisted/incomplete one, has no
		// valid `gitdir` and looks exactly like such a prunable entry, so it (and its dirty/untracked data)
		// would be deleted. Refuse before the scan and rename, for *any* position under the container — not
		// only inside a currently listed admin (a bare new child is enclosed by none). This subsumes the
		// per-admin enclosure, since every admin lives under `worktrees/`.
		let worktrees = common.join("worktrees");
		if contains(&worktrees, &request.to) {
			return Err(RelocateError::DestinationInsideRegistration {
				path: request.to.clone(),
				admin_dir: worktrees_child(&worktrees, &request.to),
			});
		}

		// Scan **every** admin registration git would list, matched by its recorded checkout path (git's own
		// listing criterion), so leaving a duplicate registration for the moved checkout is impossible. The
		// scan is fail-closed and no-follow, and ownership-agnostic:
		//   * `read_worktree_admins` (as create/remove use) includes a *symlinked* admin leaf — but such an
		//     entry cannot be read no-follow, so it is refused (`UntrustedRegistration`) rather than
		//     dereferenced or silently ignored: it might name `to`.
		//   * a physical admin's recorded path is read with `worktree_path_of`, whose error propagates (a
		//     malformed registration is a hard failure, never a silent "no match").
		//   * no ownership filter, so a *foreign* admin (broken/retargeted `commondir`) whose `gitdir` still
		//     names `to` is caught. (`admin_dirs_for`, used for the source's lock probe, applies both filters.)
		let mut stale: Vec<PathBuf> = Vec::new();
		for admin in read_worktree_admins(common)? {
			if canonical_eq(&admin, source_admin) {
				continue;
			}
			if is_leaf_symlink(&admin) {
				return Err(RelocateError::UntrustedRegistration(admin));
			}
			// Skip a stray non-admin entry under `worktrees/` (a `.DS_Store`, a directory without a `gitdir`):
			// it is not a git-listed registration and cannot name `to`, so reading its backlink (ENOTDIR /
			// absent) must not fail the whole move.
			if !is_listed_admin(&admin)? {
				continue;
			}
			if canonical_eq(&worktree_path_of(&admin)?, &request.to) {
				stale.push(admin);
			}
		}

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
	/// empty directory) vs occupied. A non-empty directory is distinguished only by a **lenient, non-fatal**
	/// read of its own `.git`: a regular gitfile that parses (`gitdir: <target>`) is a
	/// [`LinkedWorktreeCheckout`](DestinationKind::LinkedWorktreeCheckout), so the occupancy error names what
	/// collides; a malformed/absent/symlinked `.git` stays
	/// [`UnrelatedContent`](DestinationKind::UnrelatedContent) — never a parse `Failed`, matching the
	/// "already exists" occupancy git reports. A leaf symlink is seen (the trailing separator is stripped so
	/// POSIX does not resolve it) and classified as a non-directory occupant.
	fn destination_occupancy(to: &Path) -> Result<DestinationKind, LinkedWorktreeError> {
		let leaf = to.components().as_path();
		match std::fs::symlink_metadata(leaf) {
			Ok(meta) if meta.is_dir() => {
				let mut entries = std::fs::read_dir(to)
					.map_err(|e| LinkedWorktreeError::io("reading destination", to, e))?;
				if entries.next().is_some() {
					Ok(destination_git_kind(to))
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

	/// Classify a **non-empty** destination directory: a linked-worktree checkout (its `.git` is a *regular
	/// file* naming an admin — `gitdir: <target>`) versus unrelated content. Deliberately lenient and
	/// **infallible** — a malformed, absent, or symlinked `.git` (a symlink is never followed) is
	/// `UnrelatedContent`, never an error — so occupancy reports git's "already exists" rather than a parse
	/// failure. The target is not resolved; only the gitfile *shape* is recognised.
	fn destination_git_kind(to: &Path) -> DestinationKind {
		let gitfile = to.join(".git");
		match std::fs::symlink_metadata(&gitfile) {
			Ok(meta) if meta.is_file() => match std::fs::read(&gitfile) {
				Ok(bytes)
					if strip_eol_bytes(&bytes)
						.strip_prefix(b"gitdir: ".as_slice())
						.is_some_and(|target| !target.is_empty()) =>
				{
					DestinationKind::LinkedWorktreeCheckout
				}
				_ => DestinationKind::UnrelatedContent,
			},
			_ => DestinationKind::UnrelatedContent,
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

	/// The immediate child of `worktrees` on the path down to `to` — the admin slot `git worktree prune`
	/// would act on (`<worktrees>/<slot>[/…]` → `<worktrees>/<slot>`). Reported as the offending admin dir
	/// when a destination is refused for lying under the container. Falls back to `worktrees` itself when
	/// `to` resolves to the container exactly. Called only when `contains(worktrees, to)` already holds.
	fn worktrees_child(worktrees: &Path, to: &Path) -> PathBuf {
		let to_real = canonical(to);
		let mut current: &Path = &to_real;
		loop {
			match current.parent() {
				Some(parent) if canonical_eq(parent, worktrees) => return current.to_path_buf(),
				Some(parent) => current = parent,
				None => return worktrees.to_path_buf(),
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

		// `request` is the caller's request **already normalized** by `relocate` — `from` canonicalized (so the
		// rename never sees a `..`-terminated source) and `to` resolved to a stable, symlink-free absolute path
		// (pinned before the occupancy/stale checks). So the rename and pointer writes below act on paths that
		// cannot dangle once `from` is gone, without re-resolving here.
		let to = request.to.clone();

		// Before any effect, reject a destination (or admin) the pointer files cannot serialise byte-clean —
		// a non-representable path on a non-Unix platform, where `path_to_bytes` is lossy. A no-op on Unix
		// (pointers are byte-clean there), matching `create`, so the move never renames-then-corrupts a
		// backlink it cannot faithfully write.
		ensure_representable_path(&to)?;
		ensure_representable_path(admin)?;

		// Move the checkout onto the destination.
		//
		// On **Unix**, `rename` atomically **replaces an empty destination directory** — and fails
		// `ENOTEMPTY` if a concurrent write made it non-empty, so that content is never moved aside — leaving
		// `to` untouched (metadata intact) on any failure. Nothing is pre-removed or staged.
		#[cfg(unix)]
		std::fs::rename(&request.from, &to)
			.map_err(|e| LinkedWorktreeError::io("moving worktree checkout", &to, e))?;

		// On **non-Unix**, `rename` cannot replace a directory, so a validated-empty destination is removed
		// first (its `remove_dir` fails `ENOTEMPTY` on a raced non-empty one, refusing rather than clobbering)
		// and restored best-effort if the rename then fails. Its metadata is not preserved across a failed
		// move — a deferred Windows limitation, alongside WTF-8 pointer I/O.
		#[cfg(not(unix))]
		{
			let removed_empty = matches!(destination_occupancy(&to), Ok(DestinationKind::EmptyDir));
			if removed_empty {
				std::fs::remove_dir(&to)
					.map_err(|e| LinkedWorktreeError::io("clearing empty destination", &to, e))?;
			}
			if let Err(e) = std::fs::rename(&request.from, &to) {
				if removed_empty {
					let _ = std::fs::create_dir(&to);
				}
				return Err(LinkedWorktreeError::io("moving worktree checkout", &to, e).into());
			}
		}

		// Past the rename the checkout has moved: a failure now is a *partial* move — re-inspect `from` and
		// report `Incomplete` (falling back to the underlying error only if even the re-inspection fails).
		match finish_relocate(&to, admin, stale, checkout_relative, admin_relative) {
			Ok(()) => Ok(()),
			Err(err) => match inspect_structural_head(&from_query(request)).await {
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
		to: &Path,
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

		let gitfile = to.join(".git");
		let backlink = admin.join("gitdir");

		// admin `gitdir` → the checkout's new `.git`, byte-clean, its original relative/absolute style.
		// Updated **in place**, exactly as stock `git worktree move` does (verified: git rewrites this file
		// keeping its inode and mode, succeeds with the admin directory read-only, and refuses a read-only
		// `gitdir`). A temp + rename would instead fail when the admin directory is read-only — *after* the
		// checkout had already moved — and would silently reset the backlink's permissions/ACLs/xattrs when it
		// is not. The in-place write needs only file-write, preserves the file's metadata, and refuses a
		// read-only backlink. See [`update_file_in_place`] for the atomicity trade.
		let mut admin_bytes = path_to_bytes(&pointer(admin, &gitfile, admin_relative));
		admin_bytes.push(b'\n');
		update_file_in_place(&backlink, &admin_bytes)?;

		// Always rewrite the checkout's `.git` to name the (unmoved) admin, preserving its representation: a
		// relative pointer recomputed for the new depth, an absolute one rewritten to the canonical admin.
		// Rewriting even an absolute pointer matters — a valid absolute pointer that reached the admin *through
		// a path inside `from`* (via a symlink or `..`) would dangle once `from` is gone; the canonical admin
		// path never routes through the moved directory.
		//
		// Updated **in place** (not temp + rename): the checkout `.git` already exists at `to` (it moved with
		// the directory), and rewriting it in place both preserves its permissions and needs only file-write —
		// so the move completes even in a read-only checkout directory (as git does) and a read-only `.git` is
		// refused rather than silently replaced. See [`update_file_in_place`] for the atomicity trade.
		let mut checkout_bytes = b"gitdir: ".to_vec();
		checkout_bytes.extend_from_slice(&path_to_bytes(&pointer(to, admin, checkout_relative)));
		checkout_bytes.push(b'\n');
		update_file_in_place(&gitfile, &checkout_bytes)?;
		Ok(())
	}

	/// A **stable absolute** spelling of `to` that never routes through the source: its existing parent
	/// canonicalized (resolving any `..`/symlink segments, including a `<from>/..` that only resolves while
	/// `from` is present) with the final component re-attached lexically. Called **before** the rename so the
	/// parent still resolves, then reused for the rename and every pointer write, none of which can then
	/// dangle once `from` is gone. Falls back to `to` unchanged when it has no parent/final component (a root
	/// or `..`-terminated path — never a real worktree destination) or its parent cannot be canonicalized (an
	/// absent parent, which the rename itself will then reject). This yields a symlink-free *pathname*, not a
	/// pinned directory handle: it does not defend against a hostile concurrent re-symlinking of the parent
	/// after resolution — a documented deferred hardening gap (see the pinning note in `relocate`).
	fn resolve_destination(to: &Path) -> PathBuf {
		match (to.parent(), to.file_name()) {
			(Some(parent), Some(name)) => match parent.canonicalize() {
				Ok(resolved) => resolved.join(name),
				Err(_) => to.to_path_buf(),
			},
			_ => to.to_path_buf(),
		}
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

	/// Whether a pointer file records a **relative** path, parsed exactly as git and the crate's readers do
	/// (a `.git` gitfile with `strip_gitdir_prefix` true carries the `gitdir: ` prefix; an admin `gitdir` file
	/// carries the bare path). Only the format prefix and the **trailing** line terminator are stripped —
	/// leading and interior whitespace is *significant* (part of the path), so a relative pointer beginning
	/// with a space is not mis-stripped into an absolute one. The raw path is inspected without resolving it,
	/// byte-clean, so a relative pointer is seen as relative wherever it points. A missing/empty/malformed
	/// pointer reads as not-relative (the absolute default — the pointer is rewritten absolute).
	fn raw_pointer_is_relative(path: &Path, strip_gitdir_prefix: bool) -> bool {
		let Ok(bytes) = std::fs::read(path) else {
			return false;
		};
		let raw = if strip_gitdir_prefix {
			match bytes.strip_prefix(b"gitdir: ".as_slice()) {
				Some(rest) => rest,
				None => return false,
			}
		} else {
			&bytes
		};
		match path_from_bytes(strip_eol_bytes(raw)) {
			Some(pointer) => !pointer.as_os_str().is_empty() && pointer.is_relative(),
			None => false,
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::relocate;
