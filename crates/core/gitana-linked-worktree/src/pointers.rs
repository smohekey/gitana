//! Pure path + cross-pointer helpers, lifted from the CLI's `commands/worktree.rs` / `repo.rs`.
//!
//! All read-only `std::fs`, all `PathBuf`/`OsStr` (no lossy string conversion for identity). These
//! resolve the linked-worktree admin layout the way git does — the checkout's `.git` gitfile, the admin
//! `gitdir` back-pointer, the main/linked working-directory resolution, and the branch-checkout scan.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::LinkedWorktreeError;

/// The admin directories under `<common>/worktrees/` (every entry). A *missing* `worktrees` directory
/// is `Ok(empty)` — the repository simply has no linked worktrees — but a directory that exists yet
/// cannot be scanned is an error, never silently "no worktrees" (which would let a conflicting
/// registration go unseen).
fn read_worktree_admins(common: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let dir = common.join("worktrees");
	// A **symlinked** `worktrees` directory is never followed: its children would appear as ordinary admins,
	// and enumeration/branch-use would then dereference an *external* directory's `HEAD`/`gitdir`/`locked`
	// (a listing could leak an external lock reason). Surface it as malformed — fail closed — rather than
	// silently leaking or missing conflicts.
	if is_leaf_symlink(&dir) {
		return Err(LinkedWorktreeError::MalformedPointer {
			kind: crate::error::PointerKind::AdminGitdir,
			path: dir,
		});
	}
	match std::fs::read_dir(&dir) {
		Ok(entries) => {
			let mut admins = Vec::new();
			for entry in entries {
				let entry = entry.map_err(|e| LinkedWorktreeError::io("reading worktrees dir", &dir, e))?;
				admins.push(entry.path());
			}
			Ok(admins)
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
		Err(e) => Err(LinkedWorktreeError::io("reading worktrees dir", &dir, e)),
	}
}

/// Whether `admin` is an entry git would **list** as a worktree of the repository under `<common>/
/// worktrees/`: it resolves to a *directory* and carries a `gitdir` back-pointer that is a regular file.
/// A **symlinked** admin is *followed* — git lists it and treats its branch as checked out, so branch-use
/// and enumeration must too (only the ref *name* is read; no content is exposed). A stray non-directory (a
/// `.DS_Store`, a leftover lock) is ignored; an incomplete admin (no `gitdir`) is not yet a worktree; a
/// `gitdir` that is a directory / unreadable is corruption (`Err`), never silently skipped.
///
/// This is git's worktree-list membership — deliberately **independent of `commondir` ownership**: git
/// lists (and refuses another checkout of the branch of) an admin whose `commondir` is missing or points
/// elsewhere. Ownership of a *destination's registration* is a stricter, separate test ([`is_registration`]).
fn is_listed_admin(admin: &Path) -> Result<bool, LinkedWorktreeError> {
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
	match std::fs::read_to_string(&commondir) {
		Ok(raw) if !strip_eol(&raw).is_empty() => Ok(canonical_eq(
			&resolve_pointer(admin, strip_eol(&raw)),
			common,
		)),
		Ok(_) => Ok(false),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(e) => Err(LinkedWorktreeError::io(
			"reading admin commondir",
			commondir,
			e,
		)),
	}
}

/// The linked-worktree admin directories git would list under `<common>/worktrees/`, admin-name sorted
/// (git's enumeration order). Uses git's worktree-list membership (`is_listed_admin`), *not* `commondir`
/// ownership — git's `worktree list` includes a `commondir`-mismatched or symlinked admin, so enumeration
/// does too. A scan failure, or a malformed existing `gitdir`, is an error.
pub(crate) fn linked_admin_dirs(common: &Path) -> Result<Vec<PathBuf>, LinkedWorktreeError> {
	let mut admins = Vec::new();
	for admin in read_worktree_admins(common)? {
		// Enumeration reads *full* per-worktree state (HEAD/object/lock/path), so it lists only **physical**
		// admins — a **symlinked** admin (which branch-use may follow for the ref *name* alone) must not have
		// its external HEAD/lock dereferenced here (that would leak or fabricate listing data). A
		// `commondir`-mismatched but *physical* admin is still listed: it is physically within our
		// `worktrees/`, and git lists it.
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
	let text = std::fs::read_to_string(&gitdir)
		.map_err(|e| LinkedWorktreeError::io("reading admin gitdir", &gitdir, e))?;
	let pointer = Path::new(strip_eol(&text));
	if pointer.as_os_str().is_empty() {
		return Ok(true);
	}
	let git_file = if pointer.is_absolute() {
		pointer.to_path_buf()
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
/// case-insensitively when it is (typical on macOS/Windows). Absent / unreadable / unparseable → `false`.
pub(crate) fn ignorecase(common_dir: &Path) -> bool {
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
	match std::fs::read_to_string(&gitdir) {
		Ok(text) => {
			let pointer = Path::new(strip_eol(&text));
			// An empty/parent-less pointer is malformed, not a main-worktree fallback.
			if pointer.as_os_str().is_empty() {
				return Err(LinkedWorktreeError::MalformedPointer {
					kind: crate::error::PointerKind::AdminGitdir,
					path: gitdir,
				});
			}
			let git_file = if pointer.is_absolute() {
				pointer.to_path_buf()
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

/// Follow a symbolic-ref chain from `start` to its terminal ref name (`refs/heads/alias` →
/// `refs/heads/feature`), routing each hop to the per-worktree git dir or the shared common dir as git
/// does. `max_hops` is the remaining ref-read budget (git's `SYMREF_MAXDEPTH` minus any hop already spent
/// by the caller reading `HEAD`): exceeding it is a cyclic/too-deep chain, which git rejects — surfaced as
/// malformed rather than an arbitrary mid-chain name. An unreadable hop is an error, not a silent stop.
pub(crate) fn resolve_ref_terminal(
	common: &Path,
	git_dir: &Path,
	start: &str,
	max_hops: usize,
) -> Result<String, LinkedWorktreeError> {
	let malformed = |name: String| LinkedWorktreeError::MalformedPointer {
		kind: crate::error::PointerKind::Head,
		path: PathBuf::from(name),
	};
	let mut name = start.to_owned();
	let mut first = true;
	for _ in 0..max_hops {
		// `HEAD`'s *initial* target must name a full ref under `refs/` (git rejects `ref: main` / `ref: foo/
		// bar` as a repository, verified). A symref *chain* may then pass through a one-level pseudoref
		// terminal (`HEAD -> refs/heads/alias -> CUSTOM_REF`, verified), so the `refs/` requirement applies
		// only to the first name. Every hop must obey `check-ref-format` (which also blocks escaping the repo).
		if (first && !name.starts_with("refs/")) || !is_valid_refname(&name) {
			return Err(malformed(name));
		}
		first = false;
		let base = if is_per_worktree_ref(&name) {
			git_dir
		} else {
			common
		};
		match read_ref_symref(&base.join(&name))? {
			Some(next) => name = next,
			None => return Ok(name),
		}
	}
	// Budget exhausted without reaching a direct ref — a cyclic or pathologically deep symbolic-ref chain,
	// which git rejects ("too many levels of symbolic references"). Surface it as malformed rather than
	// returning an arbitrary mid-chain name that later reads as a spurious unborn/terminal ref.
	Err(malformed(name))
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
		Some(target) => resolve_ref_terminal(common, git_dir, &target, SYMREF_MAXDEPTH - 1).map(Some),
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

/// A pointer string (already stripped of its line terminator via [`strip_eol`]) resolved against `base`
/// when relative, else taken as-is (git records either form — relative under `worktree.useRelativePaths`).
/// Significant whitespace in the path is preserved (not trimmed), matching git.
fn resolve_pointer(base: &Path, pointer: &str) -> PathBuf {
	let pointer = Path::new(pointer);
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
	let content = std::fs::read_to_string(gitfile)
		.map_err(|e| LinkedWorktreeError::io("reading gitfile", gitfile, e))?;
	// git: require the `gitdir: ` prefix, then the path is *everything* after it with only the trailing
	// line terminator removed — interior newlines and other whitespace are part of the path (git accepts a
	// gitfile whose admin path legitimately contains a newline; verified). So the whole remainder is the
	// path, not just the first line — extra data therefore makes the pointer *not match* the admin (an
	// inconsistency), never silently the first line. Only a truly empty path is malformed.
	let raw = strip_eol(content.strip_prefix("gitdir: ").ok_or_else(malformed)?);
	if raw.is_empty() {
		return Err(malformed());
	}
	Ok(Some(resolve_pointer(
		gitfile.parent().ok_or_else(malformed)?,
		raw,
	)))
}

/// The `.git`-file path an admin directory's `gitdir` records (the checkout it claims), resolved to an
/// absolute path. `None` when the `gitdir` file is unreadable.
pub(crate) fn admin_gitdir_target(admin: &Path) -> Option<PathBuf> {
	let raw = std::fs::read_to_string(admin.join("gitdir")).ok()?;
	Some(resolve_pointer(admin, strip_eol(&raw)))
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
