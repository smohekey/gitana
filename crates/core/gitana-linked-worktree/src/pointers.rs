//! Pure path + cross-pointer helpers, lifted from the CLI's `commands/worktree.rs` / `repo.rs`.
//!
//! All read-only `std::fs`, all `PathBuf`/`OsStr` (no lossy string conversion for identity). These
//! resolve the linked-worktree admin layout the way git does — the checkout's `.git` gitfile, the admin
//! `gitdir` back-pointer, the main/linked working-directory resolution, and the branch-checkout scan.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::LinkedWorktreeError;

/// The raw admin entries directly under `<common>/worktrees/` (every child, no symlink policy applied).
/// A *missing* `worktrees` directory is `Ok(empty)` — the repository simply has no linked worktrees — but
/// a directory that exists yet cannot be scanned is an error. The caller has already decided what a
/// **symlinked** `worktrees/` means (see [`read_worktree_admins`] vs [`list_worktree_admins`]); this only
/// reads the directory's children.
fn read_worktree_admin_entries(dir: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	match std::fs::read_dir(dir) {
		Ok(entries) => {
			let mut admins = Vec::new();
			for entry in entries {
				let entry = entry.map_err(|e| LinkedWorktreeError::io("reading worktrees dir", dir, e))?;
				admins.push(entry.path());
			}
			Ok(admins)
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
		Err(e) => Err(LinkedWorktreeError::io("reading worktrees dir", dir, e)),
	}
}

/// The admin directories under `<common>/worktrees/` for **conflict detection** (create/remove) and
/// **branch-use** — every entry, *failing closed* on a symlinked `worktrees/` container.
///
/// A **symlinked** `worktrees` directory is never followed: its children would appear as ordinary admins,
/// and dereferencing them would read an *external* directory's `HEAD`/`gitdir`/`locked`. For these
/// callers, silently returning "no worktrees" would be unsafe — a conflicting registration behind the
/// link would go unseen, and a create/remove could then clobber or mis-target it — so the symlinked
/// container is surfaced as malformed (fail closed), never quietly empty.
///
/// **A deliberate divergence from git** (decided with Scott; see the symlink section of
/// `docs/hlds/linked-worktree-library.md`). Probed: git follows a symlinked `worktrees/` and lists what is
/// inside it, including an admin outside the repository — and the worry above is not hypothetical, a
/// regular `locked` behind such a redirect is printed verbatim (`locked VICTIM SECRET LOCK REASON`) even
/// though the attacker planted only the symlink.
///
/// **Enumeration takes the softer stance** — see [`list_worktree_admins`]: a listing has no conflict to
/// miss, so it *skips* the symlinked container (listing only the honest worktrees) rather than erroring,
/// matching how a symlinked admin *leaf* is skipped.
pub(crate) fn read_worktree_admins(common: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let dir = common.join("worktrees");
	if is_leaf_symlink(&dir) {
		return Err(LinkedWorktreeError::MalformedPointer {
			kind: crate::error::PointerKind::AdminGitdir,
			path: dir,
		});
	}
	read_worktree_admin_entries(&dir)
}

/// The admin directories under `<common>/worktrees/` for **enumeration** — every entry, *skipping* a
/// symlinked `worktrees/` container (never following it).
///
/// A listing publishes every field it reads from an admin, so it must not read one from behind a
/// redirect — but unlike conflict detection it has nothing to *miss* by skipping, so a symlinked container
/// yields no linked worktrees (the listing shows only the main worktree) rather than an error. This mirrors
/// how [`linked_admin_dirs`] drops a symlinked admin *leaf*: same taint, same skip, consistent outcome —
/// where the fail-closed [`read_worktree_admins`] would abort. The children are otherwise unfiltered here;
/// [`linked_admin_dirs`] applies the per-leaf symlink/membership test on top.
fn list_worktree_admins(common: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let dir = common.join("worktrees");
	if is_leaf_symlink(&dir) {
		return Ok(Vec::new());
	}
	read_worktree_admin_entries(&dir)
}

/// Whether `admin` is an entry git would **list** as a worktree of the repository under `<common>/
/// worktrees/`: it resolves to a *directory* and carries a `gitdir` back-pointer that is a regular file.
/// A **symlinked** admin is *followed* here — git lists it and treats its branch as checked out, so
/// **branch-use** must too (`worktree_git_dirs`, which reads only the ref *name*; no content is exposed).
/// **Enumeration does not** — [`linked_admin_dirs`] filters symlinked leaves out on top of this test,
/// because a listing publishes every field it reads from the admin. This predicate is git's membership
/// question alone; each caller decides what to do with a symlinked answer. A stray non-directory (a
/// `.DS_Store`, a leftover lock) is ignored; an incomplete admin (no `gitdir`) is not yet a worktree; a
/// `gitdir` that is a directory / unreadable is corruption (`Err`), never silently skipped.
///
/// This is git's worktree-list membership — deliberately **independent of `commondir` ownership**: git
/// lists (and refuses another checkout of the branch of) an admin whose `commondir` is missing or points
/// elsewhere. Ownership of a *destination's registration* is a stricter, separate test ([`is_registration`]).
pub(crate) fn is_listed_admin(admin: &Path) -> Result<bool, LinkedWorktreeError> {
	match std::fs::metadata(admin) {
		Ok(meta) if meta.is_dir() => {}
		Ok(_) => return Ok(false), // a stray file / a symlink to a non-directory
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
		Err(e) => return Err(LinkedWorktreeError::io("stat worktrees entry", admin, e)),
	}
	// The `gitdir` back-pointer is followed if it is itself a symlink to a regular file — git accepts that
	// (verified: `worktree list`/status still work), so `metadata` (which follows) rather than
	// `symlink_metadata`.
	let gitdir = admin.join("gitdir");
	match std::fs::metadata(&gitdir) {
		Ok(meta) if meta.is_file() => Ok(true),
		Ok(_) => Err(LinkedWorktreeError::MalformedPointer {
			kind: crate::error::PointerKind::AdminGitdir,
			path: gitdir,
		}),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(e) => Err(LinkedWorktreeError::io("stat admin gitdir", gitdir, e)),
	}
}

/// Whether `admin` is a registration **owned by** `common` — git-listed, its admin dir is **not itself a
/// symlink** (strict *physical* ownership), **and** its `commondir` (git's authoritative ownership pointer)
/// resolves to `common`. A destination's registration reads full per-worktree state (HEAD, `locked`,
/// index), so a symlinked admin — which branch-use may *follow* for the ref name — must NOT be a
/// registration: following it there would read external `HEAD`/lock/index and could return a lock file's
/// contents as the public reason. An admin whose `commondir` is missing (git defaults it to the admin
/// itself) or retargeted at another repository is that repository's worktree, not ours.
fn is_registration(common: &Path, admin: &Path) -> Result<bool, LinkedWorktreeError> {
	// Only a genuine *absence* (`NotFound`) means "not physically present"; any other metadata failure (e.g.
	// `PermissionDenied` on an unsearchable `worktrees/`) is surfaced, never mapped to "absent" — silently
	// dropping a retained registration could misreport it as safe to create.
	let physically_present = match std::fs::symlink_metadata(admin) {
		Ok(m) => !m.file_type().is_symlink(),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
		Err(e) => return Err(LinkedWorktreeError::io("stat admin", admin, e)),
	};
	Ok(physically_present && is_listed_admin(admin)? && admin_commondir_is(common, admin)?)
}

/// Whether `<admin>/commondir` resolves to `common`. Absent → `Ok(false)` (git defaults the common dir to
/// the admin, so it is not owned by `common`); an unreadable file is an `Err`.
fn admin_commondir_is(common: &Path, admin: &Path) -> Result<bool, LinkedWorktreeError> {
	let commondir = admin.join("commondir");
	match std::fs::read(&commondir) {
		// A present pointer that parses to a path names the common dir when it resolves to `common`; an
		// empty or (non-Unix) non-representable pointer is simply "not owned by `common`" (fail-closed).
		Ok(bytes) => {
			match path_from_bytes(strip_eol_bytes(&bytes)).filter(|p| !p.as_os_str().is_empty()) {
				Some(pointer) => Ok(canonical_eq(&resolve_pointer(admin, &pointer), common)),
				None => Ok(false),
			}
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(e) => Err(LinkedWorktreeError::io(
			"reading admin commondir",
			commondir,
			e,
		)),
	}
}

/// The linked-worktree admin directories **this crate enumerates** under `<common>/worktrees/`, admin-name
/// sorted (git's enumeration order). Membership is git's worktree-list test (`is_listed_admin`), *not*
/// `commondir` ownership — a `commondir`-mismatched but **physical** admin is listed, as git does. A scan
/// failure, or a malformed existing `gitdir`, is an error.
///
/// **This is git's list minus symlinked leaves — deliberately narrower than git.** git follows a symlinked
/// admin and emits it; enumeration must not, because it publishes *everything* it reads from that admin
/// (see the filter below). Branch-use is the opposite case and does follow them (`worktree_git_dirs`), so
/// "git's list membership" (`is_listed_admin`) and "what this function returns" are **not** the same set —
/// do not treat them as interchangeable.
pub(crate) fn linked_admin_dirs(common: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let mut admins = Vec::new();
	// Enumeration *skips* a symlinked `worktrees/` container (`list_worktree_admins`), listing only the
	// honest worktrees — consistent with the per-leaf skip below, and unlike the fail-closed
	// `read_worktree_admins` create/remove use.
	for admin in list_worktree_admins(common)? {
		// Enumeration reads *full* per-worktree state (HEAD/object/lock/path), so it lists only **physical**
		// admins — a **symlinked** admin (which branch-use may follow for the ref *name* alone) must not have
		// its external HEAD/lock dereferenced here (that would leak or fabricate listing data). A
		// `commondir`-mismatched but *physical* admin is still listed: it is physically within our
		// `worktrees/`, and git lists it.
		//
		// **git does list a symlinked admin leaf — probed, twice.** (Resolved object when the admin's relative
		// `commondir` `../..` still resolves through the link; a null object when the link escapes outside
		// `.git`, which is what an earlier probe misread as "git omits these".) So dropping it is a deliberate
		// divergence, kept for the same reason as the `worktrees/` case above: there is no untainted field to
		// report for an admin behind a redirect.
		//
		// The `&&` short-circuit is load-bearing: `is_listed_admin` can `Err` on a malformed `gitdir` and is
		// never reached for a symlinked admin, so evaluating both unconditionally would turn today's silent
		// skip into an enumeration-aborting error.
		if !is_leaf_symlink(&admin) && is_listed_admin(&admin)? {
			admins.push(admin);
		}
	}
	admins.sort();
	Ok(admins)
}

/// Whether `admin` sits directly under this repository's `<common>/worktrees/` — i.e. belongs to this
/// repository rather than a foreign one. Used to reject a destination whose `.git` claims an admin
/// outside the repository *before* that admin is dereferenced. A **symlinked** admin entry is rejected
/// (never owned): it could point outside the repository, so following it would read a foreign admin.
pub(crate) fn under_worktrees(common: &Path, admin: &Path) -> bool {
	let is_symlink = std::fs::symlink_metadata(admin)
		.map(|m| m.file_type().is_symlink())
		.unwrap_or(false);
	if is_symlink {
		return false;
	}
	// The admin's parent directory must *be* the repository's `worktrees` directory (by filesystem
	// identity, resolving any symlink in the path), so a real entry under a symlinked `worktrees` still
	// matches while a symlinked leaf (rejected above) does not.
	admin
		.parent()
		.is_some_and(|parent| canonical_eq(parent, &common.join("worktrees")))
}

/// Whether `admin` is *owned by* the repository at `common` — physically under its `worktrees/` **and**
/// its `commondir` (git's authoritative ownership pointer) names `common`. A checkout's claimed admin may
/// only be dereferenced (its HEAD/lock read) against `common` when owned; one sitting under `worktrees/`
/// but whose `commondir` targets *another* repository is foreign, and reading its HEAD against `common`
/// would fabricate facts for the wrong repository.
pub(crate) fn admin_owned_by(common: &Path, admin: &Path) -> Result<bool, LinkedWorktreeError> {
	Ok(under_worktrees(common, admin) && admin_commondir_is(common, admin)?)
}

/// git's **prunable** test for a linked worktree (`should_prune_worktree`): the checkout `.git` file that
/// `<admin>/gitdir` names no longer exists. git decides prunability from that pointer *target*'s existence
/// alone — tested with `lstat`, so a **dangling-symlink** `.git` still counts as present — and it never
/// reads or validates the checkout `.git`'s *contents*. So a checkout whose `.git` is foreign, broken, a
/// directory, or a symlink is a *live* listing (not prunable) as long as *something* sits at that path;
/// only a genuinely missing target is prunable. This is deliberately **weaker** than the identity check
/// [`checkout_gitfile_names`] (which inspection/removal use): listing tolerates a hijacked `.git`, but a
/// destructive operation must not. An empty `gitdir` pointer is git's "invalid gitdir file" → prunable.
pub(crate) fn admin_checkout_missing(admin: &Path) -> Result<bool, LinkedWorktreeError> {
	let gitdir = admin.join("gitdir");
	let bytes = std::fs::read(&gitdir)
		.map_err(|e| LinkedWorktreeError::io("reading admin gitdir", &gitdir, e))?;
	// An empty pointer is git's "invalid gitdir file" → prunable; a (non-Unix) non-representable pointer is
	// likewise treated as prunable (fail-closed — it names no checkout this platform can resolve).
	let Some(pointer) =
		path_from_bytes(strip_eol_bytes(&bytes)).filter(|p| !p.as_os_str().is_empty())
	else {
		return Ok(true);
	};
	let git_file = if pointer.is_absolute() {
		pointer
	} else {
		admin.join(pointer)
	};
	// `lstat` (not `stat`): a `.git` that is a *dangling* symlink still exists to git, so is not prunable.
	Ok(std::fs::symlink_metadata(&git_file).is_err())
}

/// Whether `checkout` is a valid linked-worktree checkout for `admin`: its `.git` is a **regular file**
/// (a gitfile — a symlink is never followed) that names `admin`. This is the strict *identity* check
/// inspection and removal use to tell a live checkout of *this* admin from a directory that merely exists
/// at the recorded path — **not** git's prunable/listing test, which is the weaker
/// [`admin_checkout_missing`] above (git's `worktree list` never reads the checkout `.git`'s contents). A
/// `.git` regular file that is malformed is a hard error (see [`gitfile_target`]).
pub(crate) fn checkout_gitfile_names(
	checkout: &Path,
	admin: &Path,
) -> Result<bool, LinkedWorktreeError> {
	Ok(gitfile_target(&checkout.join(".git"))?.is_some_and(|target| canonical_eq(&target, admin)))
}

/// Whether the *main* checkout at `checkout` still identifies the repository's `common` dir — the identity
/// re-check that keeps a stale main worktree from being statused. An **ordinary** main worktree's `.git`
/// *is* `common` (a directory); a `--separate-git-dir` main worktree's `.git` is a gitfile pointing at the
/// external `common`. If the checkout is later moved or replaced (the external `common` outlives it), the
/// `.git` no longer names `common` and this is `false`, so `status` refuses rather than opening the
/// replacement with the stale index. A malformed `.git` gitfile is a hard error (see [`gitfile_target`]).
pub(crate) fn main_checkout_identifies_common(
	checkout: &Path,
	common: &Path,
) -> Result<bool, LinkedWorktreeError> {
	let dotgit = checkout.join(".git");
	if let Some(target) = gitfile_target(&dotgit)? {
		return Ok(canonical_eq(&target, common)); // separate-git-dir gitfile
	}
	// Not a gitfile: an ordinary main worktree's `.git` is a *directory* equal to `common`. A `.git` that
	// is absent or a symlink (never followed) is not a valid main worktree.
	Ok(match std::fs::symlink_metadata(&dotgit) {
		Ok(meta) if meta.is_dir() => canonical_eq(&dotgit, common),
		_ => false,
	})
}

/// Resolve `path` to its real form, tolerating a not-yet-existing leaf (resolve the deepest existing
/// ancestor, then rejoin) — so a destination that does not exist yet still compares canonically.
pub(crate) fn canonical(path: &Path) -> PathBuf {
	if let Ok(resolved) = path.canonicalize() {
		return resolved;
	}
	match (path.parent(), path.file_name()) {
		(Some(parent), Some(name)) if !parent.as_os_str().is_empty() => canonical(parent).join(name),
		_ => path.to_path_buf(),
	}
}

/// Compare two paths for identity. On a **case-insensitive** filesystem (default macOS/Windows volumes)
/// `canonicalize` preserves the caller's spelling, so `/repo/WorkTree` and `/repo/worktree` — the same
/// directory git accepts interchangeably — canonicalize to *different* strings. So when both paths exist,
/// compare by filesystem identity (device + inode) first; a differing string only decides the case where
/// one side does not exist yet (an absent destination), where a canonical-string compare is the best
/// available and case cannot alias two live directories.
pub(crate) fn canonical_eq(a: &Path, b: &Path) -> bool {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
			return ma.dev() == mb.dev() && ma.ino() == mb.ino();
		}
	}
	let (ca, cb) = (canonical(a), canonical(b));
	if ca == cb {
		return true;
	}
	// At least one path does not exist — e.g. a *deleted* registered checkout, queried by a case-variant
	// path. On a case-insensitive filesystem `CaSeWt` and `casewt` are the same directory git still
	// recognizes, so fall back to an ASCII-case-insensitive compare — but only when a shared existing
	// ancestor is genuinely case-insensitive, so two distinct case-variant dirs on a case-sensitive
	// filesystem are never merged.
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		if ca
			.as_os_str()
			.as_bytes()
			.eq_ignore_ascii_case(cb.as_os_str().as_bytes())
			&& deepest_existing_is_case_insensitive(&ca)
		{
			return true;
		}
	}
	false
}

/// Whether the deepest existing ancestor of `path` is on a case-insensitive filesystem — probed with
/// `stat` only (no file creation, no side effects): a case-flipped spelling of an existing directory that
/// resolves to the same inode means the filesystem folds case. When the deepest ancestor has no ASCII
/// letter to flip, walk **further up** to a probeable ancestor (same filesystem → same case sensitivity)
/// rather than giving up. Conservative — `false` when no ancestor can be probed.
#[cfg(unix)]
fn deepest_existing_is_case_insensitive(path: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	// Start at the deepest existing ancestor, then keep climbing until one is probeable.
	let mut dir = path;
	while !dir.exists() {
		match dir.parent() {
			Some(parent) => dir = parent,
			None => return false,
		}
	}
	// Case sensitivity is a *filesystem* property, so never climb across a mount boundary — a case-sensitive
	// volume mounted below a case-insensitive one must not inherit the parent's folding. Pin the device.
	let start_dev = match std::fs::metadata(dir) {
		Ok(m) => m.dev(),
		Err(_) => return false,
	};
	loop {
		match probe_case_fold(dir) {
			Some(result) => return result,
			// Unprobeable (no ASCII letter to flip) — climb to a parent, but only on the same filesystem.
			None => match dir
				.parent()
				.and_then(|p| Some((p, std::fs::metadata(p).ok()?)))
			{
				Some((parent, meta)) if meta.dev() == start_dev => dir = parent,
				_ => return false,
			},
		}
	}
}

/// Probe whether `dir` (which must exist) is on a case-insensitive filesystem by stat-ing a case-flipped
/// spelling of its own name. `None` when it cannot be probed (no ASCII letter to flip, or a stat error);
/// `Some(true/false)` otherwise.
#[cfg(unix)]
fn probe_case_fold(dir: &Path) -> Option<bool> {
	use std::os::unix::fs::MetadataExt;
	let here = std::fs::metadata(dir).ok()?;
	let name = dir.file_name()?.to_str()?;
	let parent = dir.parent()?;
	let flipped: String = name
		.chars()
		.map(|c| {
			if c.is_ascii_uppercase() {
				c.to_ascii_lowercase()
			} else {
				c.to_ascii_uppercase()
			}
		})
		.collect();
	if flipped == name {
		return None; // nothing to flip (no ASCII letters) — cannot probe here
	}
	Some(
		std::fs::metadata(parent.join(flipped))
			.map(|m| m.dev() == here.dev() && m.ino() == here.ino())
			.unwrap_or(false),
	)
}

/// Whether the repository at `common_dir` is bare (`core.bare`), using git's boolean grammar. An
/// *invalid* `core.bare` value is a hard error (git rejects it), never silently `false`. An absent key
/// or unreadable/unparseable config falls back to `false` — a parseable config is a precondition the
/// object-format detection already enforces upstream, so this only guards the boolean itself.
pub(crate) fn is_bare(common_dir: &Path) -> Result<bool, LinkedWorktreeError> {
	let config_path = common_dir.join("config");
	let Ok(text) = std::fs::read_to_string(&config_path) else {
		return Ok(false);
	};
	let Ok(config) = gitana_config::GitConfig::parse(&text) else {
		return Ok(false);
	};
	match config.get_bool("core", None, "bare") {
		Ok(Some(bare)) => Ok(bare),
		// An unset `core.bare` defaults to **non-bare**, as git does — verified: even a bare clone with
		// `core.bare` removed reports `--is-bare-repository=false`, and a `--separate-git-dir` git dir (a
		// name other than `.git`) with no `core.bare` is non-bare too. The git-dir basename does **not**
		// imply bareness (git writes `core.bare=true` at bare init/clone, so a genuine bare repo carries it).
		Ok(None) => Ok(false),
		Err(_) => Err(LinkedWorktreeError::InvalidCoreBare(config_path)),
	}
}

/// Whether `core.ignorecase` is set for the repository at `common_dir` — git compares worktree paths
/// case-insensitively when it is (typical on macOS/Windows).
///
/// `effective` is the caller's already-merged config stack (see [`crate::WorktreeContext`]); `None`
/// falls back to the repository-local config alone. git resolves this key through its full precedence
/// stack, so a consumer that wants git's answer injects the merged config — reading `<common>/config`
/// here would miss a `core.ignorecase` set globally (the common case on macOS, where `git init` writes
/// it locally but a user may also carry it in `~/.gitconfig`).
///
/// Absent / unreadable / unparseable → `false`. This key is **cosmetic** here: it selects the
/// case-sensitivity of the worktree-listing *sort*, matching git's display order. It must never be
/// reused to fold case in a *safety* comparison — see `status::residual_untracked_paths`, which
/// deliberately matches tracked paths byte-exactly because a fold could let a case-distinct untracked
/// file be deleted.
pub(crate) fn ignorecase(effective: Option<&gitana_config::GitConfig>, common_dir: &Path) -> bool {
	if let Some(config) = effective {
		return matches!(config.get_bool("core", None, "ignorecase"), Ok(Some(true)));
	}
	let Ok(text) = std::fs::read_to_string(common_dir.join("config")) else {
		return false;
	};
	let Ok(config) = gitana_config::GitConfig::parse(&text) else {
		return false;
	};
	matches!(config.get_bool("core", None, "ignorecase"), Ok(Some(true)))
}

/// Every worktree's git directory for the repository at `common_dir`: the main worktree (`common_dir`
/// itself, unless bare) and each `<common_dir>/worktrees/<name>` that carries a `HEAD`. A scan failure
/// on an existing `worktrees` directory is an error, not silently "no linked worktrees".
pub(crate) fn worktree_git_dirs(common_dir: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let mut git_dirs = Vec::new();
	if !is_bare(common_dir)? {
		git_dirs.push(common_dir.to_path_buf());
	}
	for admin in read_worktree_admins(common_dir)? {
		// Every admin git *lists* enters the branch-use scan — git refuses another checkout of the branch of
		// any listed worktree, **including one whose `commondir` is missing/retargeted or that is symlinked**
		// (`is_listed_admin`, not the stricter ownership test). An incomplete admin (no `gitdir`) or a stray
		// non-directory is skipped, as git does, so its path is never mistaken for a checkout of the branch.
		if is_listed_admin(&admin)? {
			git_dirs.push(admin);
		}
	}
	Ok(git_dirs)
}

/// The working directory for a worktree named by its git directory (git's `get_linked_worktree` /
/// `get_main_worktree`): a linked worktree's parent-of-`.git`, or the main worktree's common dir with a
/// trailing `/.git` stripped. An admin whose `gitdir` file **exists but is malformed** (empty, or with
/// no parent directory) is a hard error — a fabricated path would mislead enumeration and inspection.
pub(crate) fn worktree_path_of(git_dir: &Path) -> Result<PathBuf, LinkedWorktreeError> {
	let gitdir = git_dir.join("gitdir");
	match std::fs::read(&gitdir) {
		Ok(bytes) => {
			// An empty/parent-less pointer, or a (non-Unix) non-representable one, is malformed — not a
			// main-worktree fallback.
			let Some(pointer) =
				path_from_bytes(strip_eol_bytes(&bytes)).filter(|p| !p.as_os_str().is_empty())
			else {
				return Err(LinkedWorktreeError::MalformedPointer {
					kind: crate::error::PointerKind::AdminGitdir,
					path: gitdir,
				});
			};
			let git_file = if pointer.is_absolute() {
				pointer
			} else {
				git_dir.join(pointer)
			};
			match git_file.parent() {
				Some(parent) => Ok(parent.to_path_buf()),
				None => Err(LinkedWorktreeError::MalformedPointer {
					kind: crate::error::PointerKind::AdminGitdir,
					path: gitdir,
				}),
			}
		}
		// No `gitdir` file — the main worktree: strip a trailing `.git` component from the common dir.
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			if git_dir.file_name() == Some(OsStr::new(".git"))
				&& let Some(parent) = git_dir.parent()
			{
				Ok(parent.to_path_buf())
			} else {
				Ok(git_dir.to_path_buf())
			}
		}
		Err(e) => Err(LinkedWorktreeError::io("reading admin gitdir", gitdir, e)),
	}
}

/// The main worktree's working directory, derived **directly** from `common` (strip a trailing `.git`) —
/// the primary git dir is not a linked admin, so its path must never be read from a `gitdir` file. git
/// ignores a stray `gitdir`/`locked` in the main `.git`; deriving the path here (and forcing the primary's
/// lock `Unlocked` at the call site) keeps a stray file from fabricating a checkout path or a phantom lock.
pub(crate) fn main_worktree_path(common: &Path) -> PathBuf {
	if common.file_name() == Some(OsStr::new(".git")) {
		common
			.parent()
			.map(Path::to_path_buf)
			.unwrap_or_else(|| common.to_path_buf())
	} else {
		common.to_path_buf()
	}
}

/// Read a ref file's symbolic target: `Ok(Some(target))` for a `ref: <target>`, `Ok(None)` for a direct
/// object id or an absent file (a packed ref is never symbolic, so a symbolic ref is always a loose
/// `ref:` file), and **`Err`** when the file exists but is unreadable or non-UTF-8 — a failure that must
/// not be mistaken for "no ref", since it could hide a branch-use conflict.
fn read_ref_symref(path: &Path) -> Result<Option<String>, LinkedWorktreeError> {
	match std::fs::symlink_metadata(path) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(LinkedWorktreeError::io("reading ref", path, e)),
		// A *symlink* ref file is a legacy symbolic ref **only when its target names a ref** — i.e. begins
		// with `refs/`, interpreted repository-relative (`.git/HEAD -> refs/heads/main`). git treats any
		// other symlink target (a relative sibling such as `refs/heads/alias -> feature`) as a *direct* ref,
		// following the link to read the object id; so `alias` is its own branch (occupying it, verified vs
		// git), not a symref to `feature`. A direct ref's terminal is the ref name itself, so return `None`.
		Ok(meta) if meta.file_type().is_symlink() => {
			let target = std::fs::read_link(path)
				.map_err(|e| LinkedWorktreeError::io("reading ref symlink", path, e))?;
			let target = target.to_string_lossy();
			Ok(target.starts_with("refs/").then(|| target.into_owned()))
		}
		// A ref path that is a **directory** is a ref *namespace*, not a ref — git permits an unborn branch
		// `foo` while `refs/heads/foo/bar` exists (so `refs/heads/foo` is a directory), e.g. after
		// `git worktree add --orphan -b foo`. Treat it as non-symbolic (a terminal/unborn ref), not an error.
		Ok(meta) if meta.is_dir() => Ok(None),
		Ok(_) => {
			let body = std::fs::read_to_string(path)
				.map_err(|e| LinkedWorktreeError::io("reading ref", path, e))?;
			// Strip trailing line terminators, then the `ref:` tag; git accepts only space/tab as the
			// separator (not VT/FF/Unicode ws), so trim only those — any other byte stays in the (then
			// invalid) target and is rejected by `resolve_ref_terminal`'s refname check.
			Ok(
				strip_eol(&body)
					.strip_prefix("ref:")
					.map(|t| t.trim_matches([' ', '\t']).to_owned()),
			)
		}
	}
}

/// git's maximum symbolic-ref resolution depth (`SYMREF_MAXDEPTH`) — at most this many ref reads before
/// git gives up with "too many levels of symbolic references".
pub(crate) const SYMREF_MAXDEPTH: usize = 5;

/// Whether `name` obeys git's `check-ref-format` rules (verified against `git check-ref-format`): no
/// component beginning with `.` or ending with `.lock`, no `..`, no `//` / leading / trailing slash, no
/// control chars / space / `~^:?*[\`, no `@{`, not a bare `@`, no trailing `.`. This both matches git
/// (a HEAD naming an *invalid* ref is rejected, never reported as a healthy unborn branch) **and**
/// preserves the security property — `..`, absolute paths, and `\` are refused, so `base.join(name)`
/// cannot escape the repository.
pub(crate) fn is_valid_refname(name: &str) -> bool {
	if name.is_empty() || name == "@" {
		return false;
	}
	if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
		return false;
	}
	if name.contains("..") || name.contains("@{") || name.ends_with('.') {
		return false;
	}
	if name.bytes().any(|b| {
		b < 0x20 || b == 0x7f || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
	}) {
		return false;
	}
	name
		.split('/')
		.all(|comp| !comp.starts_with('.') && !comp.ends_with(".lock"))
}

/// A ref that lives in the *per-worktree* namespace (read from the worktree git dir) rather than the
/// shared common dir — git's per-worktree refs.
fn is_per_worktree_ref(name: &str) -> bool {
	// A **pseudoref** (`HEAD`, `ORIG_HEAD`, a custom `CUSTOM-REF`) lives per-worktree, as do the
	// `refs/{worktree,bisect,rewritten}/` namespaces; everything else is shared in the common dir. A pseudoref
	// is git's `is_pseudoref_syntax`: a top-level name of only uppercase letters, `_`, and `-` — which **excludes
	// digits** (so `CUSTOM1` is shared), matching git and the `WorktreeFileStore` routing that resolves the
	// object.
	is_pseudoref_name(name)
		|| name.starts_with("refs/worktree/")
		|| name.starts_with("refs/bisect/")
		|| name.starts_with("refs/rewritten/")
}

/// git's `is_pseudoref_syntax`: a non-empty top-level name of only ASCII uppercase letters, `_`, and `-`
/// (digits excluded). Must stay in lock-step with `gitana_file_store_local`'s equivalent routing predicate.
fn is_pseudoref_name(name: &str) -> bool {
	!name.is_empty()
		&& name
			.bytes()
			.all(|b| b.is_ascii_uppercase() || b == b'_' || b == b'-')
}

/// Where the `start` name being resolved came from — decides how a malformed/too-deep chain is reported,
/// so the failure names the right thing and never echoes on-disk content.
#[derive(Clone, Copy)]
pub(crate) enum RefSource<'a> {
	/// The chain roots at a worktree's `HEAD` file. A malformation is a [`MalformedPointer`] naming that
	/// `<git_dir>/HEAD` — never the offending target *name* (which for a followed symlinked admin is content
	/// read from behind the redirect: probed as `malformed HEAD pointer at TOP_SECRET_LEAKED`).
	///
	/// [`MalformedPointer`]: LinkedWorktreeError::MalformedPointer
	Head,
	/// The chain roots at a **caller-supplied** requested branch (short name). Only a malformation of the
	/// *initial* argument is an [`InvalidRequestedBranch`] — a bad argument, never blamed on the healthy
	/// `HEAD`, and the name being the caller's own discloses nothing. A malformation *after* a symref hop is
	/// on-disk corruption in a repository ref file (a valid branch whose symref target is broken/cyclic),
	/// reported as a [`MalformedPointer`] of kind [`Ref`](crate::error::PointerKind::Ref) naming the branch's
	/// root ref file — not the caller and not a read target name.
	///
	/// [`InvalidRequestedBranch`]: LinkedWorktreeError::InvalidRequestedBranch
	/// [`MalformedPointer`]: LinkedWorktreeError::MalformedPointer
	RequestedBranch(&'a str),
}

/// Follow a symbolic-ref chain from `start` to its terminal ref name (`refs/heads/alias` →
/// `refs/heads/feature`), routing each hop to the per-worktree git dir or the shared common dir as git
/// does. `max_hops` is the remaining ref-read budget (git's `SYMREF_MAXDEPTH` minus any hop already spent
/// by the caller reading `HEAD`): exceeding it is a cyclic/too-deep chain, which git rejects — surfaced as
/// malformed rather than an arbitrary mid-chain name. An unreadable hop is an error, not a silent stop.
///
/// `source` decides how a malformation is reported (see [`RefSource`]): the error **never** carries the
/// offending ref *name*, only the resolved-from HEAD file or the caller's own branch argument. Rendering
/// the name would echo file content — and when `git_dir` is a followed symlinked admin, that content is
/// read from behind the redirect.
pub(crate) fn resolve_ref_terminal(
	common: &Path,
	git_dir: &Path,
	start: &str,
	source: RefSource<'_>,
	max_hops: usize,
) -> Result<String, LinkedWorktreeError> {
	// The branch's root ref file, for a `RequestedBranch` corruption *after* the first hop — the chain the
	// caller asked to resolve, named by its own (caller-supplied) branch ref, never a read target name.
	let branch_root = || {
		if is_per_worktree_ref(start) {
			git_dir
		} else {
			common
		}
		.join(start)
	};
	// The safe path to attribute a failure to, for a source whose `name` is untrusted content — the HEAD
	// file, or (for a requested branch) the caller's own branch ref. Never a `base.join(name)` that embeds a
	// name read from a possibly-symlinked admin.
	let safe_root = || match source {
		RefSource::Head => git_dir.join("HEAD"),
		RefSource::RequestedBranch(_) => branch_root(),
	};
	// `first` distinguishes a bad *initial* value (the caller's argument, or `HEAD`'s target) from on-disk
	// corruption discovered after following a symref hop.
	let malformed = |first: bool| match source {
		RefSource::Head => LinkedWorktreeError::MalformedPointer {
			kind: crate::error::PointerKind::Head,
			path: git_dir.join("HEAD"),
		},
		RefSource::RequestedBranch(name) if first => {
			LinkedWorktreeError::InvalidRequestedBranch(name.to_owned())
		}
		RefSource::RequestedBranch(_) => LinkedWorktreeError::MalformedPointer {
			kind: crate::error::PointerKind::Ref,
			path: branch_root(),
		},
	};
	// A ref-read *I/O* failure must not render `base.join(&name)` either: `name` is content read from disk
	// (always, for a `Head` root; after the first hop, for a branch) — for a followed symlinked admin, from
	// behind the redirect. A syntactically-valid-but-unopenable target (e.g. a component over `NAME_MAX`)
	// would otherwise leak through the `Io` path (probed). Rebind such an error's path to the safe root; the
	// caller's *own* first branch read stays as-is (its path is the caller's argument, not disclosure).
	let redact_read = |e: LinkedWorktreeError, first_hop: bool| match e {
		LinkedWorktreeError::Io {
			context,
			source: io,
			..
		} if !(matches!(source, RefSource::RequestedBranch(_)) && first_hop) => LinkedWorktreeError::Io {
			context,
			source: io,
			path: safe_root(),
		},
		other => other,
	};
	let mut name = start.to_owned();
	let mut first = true;
	for _ in 0..max_hops {
		// `HEAD`'s *initial* target must name a full ref under `refs/` (git rejects `ref: main` / `ref: foo/
		// bar` as a repository, verified). A symref *chain* may then pass through a one-level pseudoref
		// terminal (`HEAD -> refs/heads/alias -> CUSTOM_REF`, verified), so the `refs/` requirement applies
		// only to the first name. Every hop must obey `check-ref-format` (which also blocks escaping the repo).
		if (first && !name.starts_with("refs/")) || !is_valid_refname(&name) {
			return Err(malformed(first));
		}
		let first_hop = first;
		first = false;
		let base = if is_per_worktree_ref(&name) {
			git_dir
		} else {
			common
		};
		match read_ref_symref(&base.join(&name)).map_err(|e| redact_read(e, first_hop))? {
			Some(next) => name = next,
			None => return Ok(name),
		}
	}
	// Budget exhausted without reaching a direct ref — a cyclic or pathologically deep symbolic-ref chain,
	// which git rejects ("too many levels of symbolic references"). It is always on-disk (a cycle needs ref
	// files), so `first` is `false`. Surface it as malformed rather than an arbitrary mid-chain name.
	Err(malformed(false))
}

/// The *terminal* branch ref a worktree's `HEAD` resolves to (following the symbolic-ref chain), or
/// `None` when `HEAD` is detached or absent. git treats the terminal ref as the checked-out branch, so
/// the branch-use scan compares against it. An unreadable `HEAD`/ref is an error.
fn resolve_head_branch(
	common: &Path,
	git_dir: &Path,
) -> Result<Option<String>, LinkedWorktreeError> {
	match read_ref_symref(&git_dir.join("HEAD"))? {
		// `HEAD` itself consumed one hop of git's budget, so the chain from its target has `MAXDEPTH - 1` left.
		Some(target) => resolve_ref_terminal(
			common,
			git_dir,
			&target,
			RefSource::Head,
			SYMREF_MAXDEPTH - 1,
		)
		.map(Some),
		None => Ok(None), // detached or no HEAD — not on a branch
	}
}

/// The working directory of a worktree whose `HEAD` is the symbolic ref `branch`, skipping the checkout
/// at `exclude_checkout` (a *checkout path* to ignore — typically the destination being inspected).
/// git normally checks a branch out in at most one worktree, but `worktree add --force` permits
/// duplicates, so the scan continues past the excluded checkout to find *another* one carrying the
/// branch (a genuine branch-use conflict) rather than stopping at the first match.
pub(crate) fn branch_checkout_location(
	common_dir: &Path,
	branch: &str,
	exclude_checkout: Option<&Path>,
) -> Result<Option<PathBuf>, LinkedWorktreeError> {
	for candidate in worktree_git_dirs(common_dir)? {
		// git's shared-symref test peels each worktree's HEAD to its terminal branch but compares against
		// the *requested* ref name **unpeeled** — so a request for `alias` (→ feature) does NOT conflict
		// with a worktree on `feature`, while a request for `feature` does. Match that: peel the candidate
		// HEAD, compare to the raw `branch`. An unreadable HEAD is an error, so a conflict is never missed.
		if resolve_head_branch(common_dir, &candidate)? == Some(branch.to_owned()) {
			// The main worktree (`candidate == common_dir`) derives its path directly from `common` — never
			// from a `gitdir` file, which for the primary is a *stray* file git ignores (reading it would
			// fabricate a checkout path and could drop the primary from the scan, missing a real conflict). A
			// **symlinked** admin's `gitdir` is NOT dereferenced: this scan reads only the ref *name*, and a
			// crafted external admin could otherwise leak its `gitdir` file contents as `other_checkout`. The
			// branch is genuinely occupied, so the conflict is reported at the admin's own owned location; its
			// checkout is external and thus never the excluded destination.
			let path = if candidate.as_path() == common_dir {
				main_worktree_path(common_dir)
			} else if is_leaf_symlink(&candidate) {
				candidate.clone()
			} else {
				worktree_path_of(&candidate)?
			};
			// Skip the excluded checkout by filesystem identity (case-insensitive-safe).
			if !exclude_checkout.is_some_and(|ex| canonical_eq(ex, &path)) {
				return Ok(Some(path));
			}
		}
	}
	Ok(None)
}

/// Strip **all** trailing line terminators (`\n`/`\r`) — and only those. git removes every trailing CR/LF
/// from a pointer record (a gitfile ending in multiple newlines or a bare CR still resolves) but preserves
/// any **other** trailing whitespace as part of the path — a repository whose directory name ends in a
/// space is git-legal (its gitfile is `gitdir: /d/meta \n`, and git resolves `/d/meta ` with the space).
/// `.trim()` would corrupt such a path, so it is never used.
fn strip_eol(s: &str) -> &str {
	s.trim_end_matches(['\n', '\r'])
}

/// Strip a trailing line terminator (`\n`/`\r`) from raw pointer-file bytes — the byte-level counterpart of
/// [`strip_eol`], so a pointer path is parsed without a lossy UTF-8 round-trip.
pub(crate) fn strip_eol_bytes(bytes: &[u8]) -> &[u8] {
	let mut end = bytes.len();
	while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
		end -= 1;
	}
	&bytes[..end]
}

/// A path parsed from raw pointer-file bytes. **Byte-clean on Unix** (`OsStrExt::from_bytes`), so a
/// non-UTF-8 identity path round-trips exactly (requirement: native paths accepted without UTF-8
/// conversion). On non-Unix, where byte-clean (WTF-8) pointer I/O is a deferred follow-up, this **fails
/// closed** — `None` for bytes that are not valid UTF-8 — rather than lossily map them to a *different*
/// path (which could resolve to the wrong repository); the caller then treats `None` as malformed metadata.
pub(crate) fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		Some(PathBuf::from(OsStr::from_bytes(bytes)))
	}
	#[cfg(not(unix))]
	{
		std::str::from_utf8(bytes).ok().map(PathBuf::from)
	}
}

/// An `OsString` built from raw bytes for the **write** side (constructing the admin-directory name).
/// Byte-clean on Unix; on non-Unix a lossy UTF-8 rendering — safe because `create`'s representability
/// preflight rejects a non-UTF-8 *destination* there before anything is written, so a non-UTF-8 basename
/// never actually reaches a write. (Reads use the fail-closed [`path_from_bytes`] instead.)
pub(crate) fn os_string_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		OsStr::from_bytes(bytes).to_os_string()
	}
	#[cfg(not(unix))]
	{
		std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
	}
}

/// The raw pointer-file bytes for a path — the inverse of [`path_from_bytes`], used to serialise the admin
/// `gitdir` back-pointer and the checkout `.git` gitfile without losing a non-UTF-8 byte.
pub(crate) fn path_to_bytes(path: &Path) -> Vec<u8> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		path.as_os_str().as_bytes().to_vec()
	}
	#[cfg(not(unix))]
	{
		path.to_string_lossy().into_owned().into_bytes()
	}
}

/// A unique temp sibling of `path` (`<name>.tmp.<pid>.<seq>`) for a write-then-rename publish. Unique per
/// process and per call (a monotonic counter), so concurrent writes never collide on the same temp name.
pub(crate) fn temp_sibling(path: &Path) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let mut name = path
		.file_name()
		.map(|n| n.to_os_string())
		.unwrap_or_default();
	name.push(format!(".tmp.{}.{}", std::process::id(), seq));
	path.with_file_name(name)
}

/// Create `path` **exclusively** (`O_CREAT | O_EXCL`, never clobbering an existing file), write `contents`,
/// and `fsync` — so a torn write is never published and the bytes are durable before the rename.
fn write_and_sync(path: &Path, contents: &[u8]) -> Result<(), LinkedWorktreeError> {
	use std::io::Write as _;
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(path)
		.map_err(|e| LinkedWorktreeError::io("creating temp file", path, e))?;
	file
		.write_all(contents)
		.map_err(|e| LinkedWorktreeError::io("writing temp file", path, e))?;
	file
		.sync_all()
		.map_err(|e| LinkedWorktreeError::io("syncing temp file", path, e))
}

/// Publish `contents` at `path` atomically: fully write an exclusive temp sibling, then `rename` it onto
/// `path` (replacing) — a reader never observes a torn pointer, and a crash leaves the target absent (a
/// classifiable partial state) rather than a half-written file (a malformed-pointer hard error). Shared by
/// `create` and `relocate`, which both publish pointer files.
pub(crate) fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<(), LinkedWorktreeError> {
	let tmp = temp_sibling(path);
	write_and_sync(&tmp, contents)?;
	std::fs::rename(&tmp, path).map_err(|e| {
		let _ = std::fs::remove_file(&tmp);
		LinkedWorktreeError::io("publishing admin file", path, e)
	})
}

/// Rewrite the contents of an **existing** pointer file in place, preserving its permissions and metadata —
/// exactly as stock `git worktree move` rewrites both worktree pointers (verified: git keeps each file's
/// inode and mode, succeeds when the containing directory is read-only, and refuses a read-only pointer).
/// Unlike [`write_file_atomic`], this opens the existing file for truncating write rather than renaming a
/// fresh temp over it, so it (a) needs only *file*-write permission and succeeds even when the containing
/// directory is read-only (mode `0555`), where the temp-sibling approach would fail *after* the checkout was
/// already renamed and wrongly report the move incomplete; and (b) refuses a read-only pointer (mode `0444`)
/// with `EACCES` — matching git and the prior CLI — rather than replacing it and silently clearing its
/// read-only bit (and its ACLs/xattrs). `create: false` never creates the file, so a genuinely absent
/// pointer surfaces as an error, not a stray new file in a read-only tree.
///
/// The trade against `write_file_atomic` is atomicity: a torn write (ENOSPC mid-write) can leave a partial
/// pointer rather than the prior contents. Both relocate pointers accept it because stock git does — git
/// truncates and rewrites these files in place too, so matching its permission and metadata semantics
/// (the divergence the review flagged) outweighs a torn-write guarantee git itself does not provide; a
/// partial pointer is in any case repairable with `git worktree repair`. `write_file_atomic` remains for
/// `create`, which publishes a *new* registration where the temp + rename (never clobbering an existing
/// file) is the right primitive.
///
/// **Never follows a symlinked pointer.** git writes these as regular files; a symlink in their place is
/// corruption or an attack, and following it would truncate and rewrite a file *outside* the admin. Because
/// a plain pre-check races a concurrent swap (and this crate carries no `O_NOFOLLOW` binding), the file is
/// opened **without** `O_TRUNC` and the opened descriptor's identity is compared against the pre-open
/// `lstat`: a symlink swapped in between is followed to a *different* inode, so the mismatch is caught and
/// the write refused **before** any truncation — a redirected pointer can never clobber its target.
pub(crate) fn update_file_in_place(
	path: &Path,
	contents: &[u8],
) -> Result<(), LinkedWorktreeError> {
	use std::io::Write as _;

	let pre = std::fs::symlink_metadata(path)
		.map_err(|e| LinkedWorktreeError::io("stat pointer file for update", path, e))?;
	if pre.file_type().is_symlink() {
		return Err(LinkedWorktreeError::io(
			"refusing to update a symlinked pointer file",
			path,
			std::io::Error::from(std::io::ErrorKind::InvalidInput),
		));
	}

	// No `.truncate(true)`: truncation must wait until the opened descriptor is confirmed to be the very
	// regular file just `lstat`ed, so a symlink swapped in after the `lstat` cannot have its target clobbered.
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create(false)
		.open(path)
		.map_err(|e| LinkedWorktreeError::io("opening pointer file for update", path, e))?;

	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt as _;
		let opened = file
			.metadata()
			.map_err(|e| LinkedWorktreeError::io("stat opened pointer file", path, e))?;
		if !opened.file_type().is_file() || opened.dev() != pre.dev() || opened.ino() != pre.ino() {
			return Err(LinkedWorktreeError::io(
				"pointer file changed identity during update",
				path,
				std::io::Error::from(std::io::ErrorKind::InvalidInput),
			));
		}
	}

	file
		.set_len(0)
		.map_err(|e| LinkedWorktreeError::io("truncating pointer file", path, e))?;
	file
		.write_all(contents)
		.map_err(|e| LinkedWorktreeError::io("updating pointer file", path, e))?;
	file
		.sync_all()
		.map_err(|e| LinkedWorktreeError::io("syncing pointer file", path, e))
}

/// Ensure a path can round-trip the (byte-clean) pointer I/O before any state is written. On **Unix** the
/// pointers are byte-clean, so this is a no-op — a non-UTF-8 path is accepted. On **non-Unix**, where
/// [`path_to_bytes`] still falls back to a *lossy* UTF-8 rendering, a non-UTF-8 path would serialize to a
/// back-pointer that no longer identifies the destination; reject it here (before any state is written) so
/// the caller never mutates state it would then fail to establish. Shared by `create` and `relocate`, which
/// both write these pointers. Windows WTF-8 pointer I/O is a deferred follow-up.
#[cfg(unix)]
pub(crate) fn ensure_representable_path(_path: &Path) -> Result<(), LinkedWorktreeError> {
	Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_representable_path(path: &Path) -> Result<(), LinkedWorktreeError> {
	// Check the **resolved** form the pointer files will actually record — a symlink/junction can resolve a
	// UTF-8 lexical path to a non-representable one, which would then serialize lossily *after* the writes.
	// Rejecting it here keeps the operation side-effect-free on failure.
	if resolved_for_pointers(path).to_str().is_some() {
		Ok(())
	} else {
		Err(LinkedWorktreeError::io(
			"non-UTF-8 path is unsupported on this platform (byte-clean pointer I/O is Unix-only)",
			path,
			std::io::Error::from(std::io::ErrorKind::InvalidInput),
		))
	}
}

/// The form of `path` the pointer files will actually record — its deepest existing ancestor canonicalized
/// (so a symlinked parent is resolved to its real target, exactly as `create_dir_all` + `canonicalize`
/// would), with the still-absent tail appended lexically. Used only by the non-Unix representability
/// preflight; on Unix the pointers are byte-clean so no such check is needed.
#[cfg(not(unix))]
fn resolved_for_pointers(path: &Path) -> PathBuf {
	use std::path::Component;
	let mut resolved = PathBuf::new();
	for component in path.components() {
		match component {
			Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
			Component::RootDir => resolved.push(Component::RootDir.as_os_str()),
			Component::CurDir => {}
			Component::ParentDir => {
				resolved.pop();
			}
			Component::Normal(name) => {
				resolved.push(name);
				if let Ok(canonical) = resolved.canonicalize() {
					resolved = canonical;
				}
			}
		}
	}
	resolved
}

/// A pointer path (already stripped of its line terminator) resolved against `base` when relative, else
/// taken as-is (git records either form — relative under `worktree.useRelativePaths`). Significant
/// whitespace in the path is preserved (not trimmed), matching git.
fn resolve_pointer(base: &Path, pointer: &Path) -> PathBuf {
	if pointer.is_absolute() {
		pointer.to_path_buf()
	} else {
		base.join(pointer)
	}
}

/// The `gitdir: <path>` target a checkout `.git` gitfile records, resolved to an absolute path (the
/// admin directory the checkout claims). `Ok(None)` when `.git` is absent, a directory (an ordinary
/// repo, not a gitfile), or a symlink (not followed). A `.git` **regular file** must be a valid gitfile:
/// git requires the exact `gitdir: ` prefix (with the space) and takes the **entire remaining body**
/// (trailing whitespace trimmed) as the path — so a first line `gitdir: <admin>` followed by more
/// non-whitespace data is a path git rejects (`not a git repository`), *not* silently the first line.
/// A malformed gitfile is a `MalformedPointer` error rather than silently "not a checkout". Only the
/// trailing line terminator is stripped; a path's own trailing whitespace is preserved (git-legal).
pub(crate) fn gitfile_target(gitfile: &Path) -> Result<Option<PathBuf>, LinkedWorktreeError> {
	let meta = match std::fs::symlink_metadata(gitfile) {
		Ok(meta) => meta,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(LinkedWorktreeError::io("stat gitfile", gitfile, e)),
	};
	if !meta.is_file() {
		return Ok(None);
	}
	let malformed = || LinkedWorktreeError::MalformedPointer {
		kind: crate::error::PointerKind::GitFile,
		path: gitfile.to_path_buf(),
	};
	let content =
		std::fs::read(gitfile).map_err(|e| LinkedWorktreeError::io("reading gitfile", gitfile, e))?;
	// git: require the `gitdir: ` prefix, then the path is *everything* after it with only the trailing
	// line terminator removed — interior newlines and other whitespace are part of the path (git accepts a
	// gitfile whose admin path legitimately contains a newline; verified). So the whole remainder is the
	// path, not just the first line — extra data therefore makes the pointer *not match* the admin (an
	// inconsistency), never silently the first line. Only a truly empty path is malformed. Parsed
	// byte-clean, so a non-UTF-8 admin path round-trips.
	let raw = strip_eol_bytes(
		content
			.strip_prefix(b"gitdir: ".as_slice())
			.ok_or_else(malformed)?,
	);
	if raw.is_empty() {
		return Err(malformed());
	}
	// A (non-Unix) non-representable pointer is malformed metadata, not a lossily-mapped different path.
	let pointer = path_from_bytes(raw).ok_or_else(malformed)?;
	Ok(Some(resolve_pointer(
		gitfile.parent().ok_or_else(malformed)?,
		&pointer,
	)))
}

/// The `.git`-file path an admin directory's `gitdir` records (the checkout it claims), resolved to an
/// absolute path. `None` when the `gitdir` file is unreadable or (non-Unix) not representable.
pub(crate) fn admin_gitdir_target(admin: &Path) -> Option<PathBuf> {
	let bytes = std::fs::read(admin.join("gitdir")).ok()?;
	let pointer = path_from_bytes(strip_eol_bytes(&bytes))?;
	Some(resolve_pointer(admin, &pointer))
}

/// Every admin directory under `<common>/worktrees/*` whose recorded checkout is `target` — normally
/// zero or one, but **more than one indicates corruption** (a duplicate registration for one
/// destination), which the caller surfaces as an identity conflict rather than silently taking the
/// first. A scan failure, or a malformed existing pointer, is an error.
pub(crate) fn admin_dirs_for(
	common: &Path,
	target: &Path,
) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	// A destination that is itself a **leaf symlink** is never a registered worktree (it is `OtherFsObject`,
	// a destination conflict). `canonical_eq` would otherwise follow it to a registered checkout and match,
	// so inspection/status would read the alias target's `.git`/HEAD/lock — violating the no-follow boundary.
	if is_leaf_symlink(target) {
		return Ok(Vec::new());
	}
	let mut matches = Vec::new();
	for admin in read_worktree_admins(common)? {
		// Only a real registration of this repository (regular-file `gitdir` back-pointer *and* a
		// `commondir` naming `common`) is a candidate; a malformed existing pointer surfaces as a hard error
		// rather than a silently-missed registration.
		if is_registration(common, &admin)? && canonical_eq(&worktree_path_of(&admin)?, target) {
			matches.push(admin);
		}
	}
	Ok(matches)
}

/// Whether `path` is itself a symlink (its final component), without following it.
pub(crate) fn is_leaf_symlink(path: &Path) -> bool {
	// Strip trailing separators (and normalise `.`) via `components`: a trailing `/` — `.../wt-link/` — makes
	// `symlink_metadata` *follow* the leaf symlink (POSIX trailing-slash semantics), hiding it and letting a
	// later canonical delete resolve to, and destroy, the symlink's target. Stat the bare leaf so the symlink
	// itself is seen (no-follow).
	let leaf = path.components().as_path();
	std::fs::symlink_metadata(leaf)
		.map(|m| m.file_type().is_symlink())
		.unwrap_or(false)
}

#[cfg(all(test, unix))]
mod byte_pointer_tests {
	use super::{path_from_bytes, path_to_bytes, strip_eol_bytes};
	use std::os::unix::ffi::OsStrExt;
	use std::path::Path;

	#[test]
	fn path_bytes_round_trip_including_non_utf8() {
		// A non-UTF-8 identity path survives serialize→parse exactly (no UTF-8 conversion), so a pointer
		// file records and reads back the native path — the requirement byte-clean pointer I/O satisfies.
		for raw in [
			b"/tmp/wt".as_slice(),
			b"/tmp/wt\xffx",       // lone 0xff — invalid UTF-8, legal Unix byte
			b"/p\xff/q\xfer/.git", // multiple non-UTF-8 bytes across components
			b"/tmp/has space/and\ttab",
		] {
			let path = Path::new(std::ffi::OsStr::from_bytes(raw));
			assert_eq!(path_to_bytes(path), raw, "serialize preserves bytes");
			// On Unix `path_from_bytes` is always `Some` (byte-clean).
			assert_eq!(
				path_from_bytes(raw).unwrap().as_os_str().as_bytes(),
				raw,
				"parse preserves bytes"
			);
			assert_eq!(
				&path_from_bytes(&path_to_bytes(path)).unwrap(),
				path,
				"round-trip is identity"
			);
		}
	}

	#[test]
	fn strip_eol_bytes_trims_only_trailing_terminators() {
		assert_eq!(strip_eol_bytes(b"/tmp/wt\n"), b"/tmp/wt");
		assert_eq!(strip_eol_bytes(b"/tmp/wt\r\n"), b"/tmp/wt");
		assert_eq!(strip_eol_bytes(b"/tmp/wt"), b"/tmp/wt");
		// An interior newline (a git-legal path byte) is preserved.
		assert_eq!(strip_eol_bytes(b"/tmp/a\nb\n"), b"/tmp/a\nb");
		// A trailing non-UTF-8 byte is not a terminator.
		assert_eq!(strip_eol_bytes(b"/tmp/wt\xff"), b"/tmp/wt\xff");
	}
}
