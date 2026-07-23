//! `gta worktree` — create, list, and remove linked working trees (`git worktree add/list/remove`).
//!
//! gta already *operates inside* a linked worktree (see [`crate::repo`]); this command *creates* the
//! layout git reads. A linked worktree is an admin directory `<common>/worktrees/<name>/` holding the
//! per-worktree files (`HEAD`, `index`, `commondir` → the shared `.git`, `gitdir` → the checkout's
//! `.git` file) plus a checkout whose `.git` is a file pointing back at that admin directory.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use gitana_linked_worktree::{
	BranchName, CheckoutTarget, CreateError, CreateRequest, LockState, ProtectionReason, RemoveError,
	RemoveOutcome, RemovePolicy, RemoveRequest, RepositoryId, WorktreeClassification,
	WorktreeContext, WorktreeEntry, WorktreeObjectId, WorktreeRole,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_porcelain::Identity;
use gitana_repository::Repository;

use crate::Backend;
use crate::dispatch::detect_algorithm;
use crate::identity::CliIdentity;
use crate::repo;

/// A `gta worktree` operation.
pub enum Action {
	/// Create a linked worktree at `path`, checking out `commit_ish` (default `HEAD`). With no branch
	/// flag and no `commit_ish`, DWIMs a new branch named after the path's basename (git's default).
	Add {
		path: PathBuf,
		commit_ish: Option<String>,
		/// A branch to create/switch to (`-b`/`-B <name>`).
		branch: Option<String>,
		/// Force-create the `-B` branch even if it exists.
		force_branch: bool,
		/// Check out a detached `HEAD` rather than a branch (`--detach`).
		detach: bool,
	},
	/// List the repository's worktrees (the main worktree first, then the linked ones).
	List { porcelain: bool },
	/// Remove the linked worktree at `path`. `force` is a count (git's repeatable `-f`): one force
	/// removes a dirty worktree, two removes a locked one.
	Remove { path: PathBuf, force: u8 },
	/// Lock the worktree at `path` (write `<admin>/locked`), recording an optional `reason`. A locked
	/// worktree is protected from `prune` and needs a second `-f` to `remove`.
	Lock {
		path: PathBuf,
		reason: Option<String>,
	},
	/// Unlock the worktree at `path` (remove `<admin>/locked`).
	Unlock { path: PathBuf },
	/// Prune the admin directories of worktrees whose checkout is gone (honouring locks). `dry_run`
	/// (`-n`) reports without removing; `verbose` (`-v`) reports each removal; `expire` keeps a stale
	/// worktree whose per-worktree `index` is newer than the given time.
	Prune {
		dry_run: bool,
		verbose: bool,
		expire: Option<String>,
	},
	/// Move the linked worktree at `worktree` to `new_path`. `force` is a count (git's repeatable `-f`):
	/// two forces move a locked worktree, one moves onto a path registered to a since-deleted worktree.
	Move {
		worktree: PathBuf,
		new_path: PathBuf,
		force: u8,
	},
	/// Repair the cross-pointers of the worktrees at `paths` (default: the current worktree) after a
	/// manual move of a checkout or the main worktree.
	Repair { paths: Vec<PathBuf> },
}

/// Manage the repository's linked working trees.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	match action {
		Action::Add {
			path,
			commit_ish,
			branch,
			force_branch,
			detach,
		} => {
			add(
				cwd,
				&path,
				commit_ish.as_deref(),
				branch.as_deref(),
				force_branch,
				detach,
			)
			.await
		}
		Action::List { porcelain } => list(cwd, porcelain).await,
		Action::Remove { path, force } => remove(cwd, &path, force).await,
		Action::Lock { path, reason } => lock(cwd, &path, reason.as_deref()).await,
		Action::Unlock { path } => unlock(cwd, &path).await,
		Action::Prune {
			dry_run,
			verbose,
			expire,
		} => prune(cwd, dry_run, verbose, expire.as_deref()).await,
		Action::Move {
			worktree,
			new_path,
			force,
		} => move_worktree(cwd, &worktree, &new_path, force).await,
		Action::Repair { paths } => repair(cwd, &paths).await,
	}
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

async fn add(
	cwd: &Path,
	path: &Path,
	commit_ish: Option<&str>,
	branch: Option<&str>,
	force_branch: bool,
	detach: bool,
) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let common = &found.common_dir;
	let target = absolute(cwd, path);

	// Two cheap pre-checks kept CLI-side to preserve git's precedence (the destination is checked *before*
	// the checkout target is resolved) and its exact messages. Delegating flips that order — a name/commit
	// error would otherwise precede the library's destination check — so keep these here:
	//   - git refuses an existing, non-empty destination (an empty directory is fine);
	//   - a path already registered under `.git/worktrees` (even one whose checkout was deleted) must not be
	//     re-added, or the repository ends up with two admin entries for one path.
	if target.exists() && dir_non_empty(&target)? {
		bail!("'{}' already exists", path.display());
	}
	if admin_dir_for(common, &canonical(&target)).is_some() {
		bail!(
			"'{}' is a missing but already registered worktree",
			path.display()
		);
	}

	// DWIM resolution stays CLI-side (git's `worktree add` mode inference): resolve the start point + checkout
	// mode against the current repository (its refs/objects are shared with the new worktree), producing the
	// explicit `CheckoutTarget` the library takes plus a `Label` for the "Preparing worktree" line. The
	// env-aware committer (honouring `GIT_COMMITTER_*`, incl. `DATE`) is resolved here too, the same way
	// clone/fetch/pull do — the library records it on every reflog line it writes.
	let (checkout_target, label, committer) = match detect_algorithm(common)? {
		HashKind::Sha1 => {
			let repo = repo::open_generic::<Sha1>(&found.git_dir, common).await?;
			let (target, label) = plan_checkout::<Sha1>(
				&repo,
				&target,
				commit_ish,
				branch,
				force_branch,
				detach,
				HashKind::Sha1,
			)
			.await?;
			let committer = CliIdentity::new(&repo).committer_or_default().await?;
			(target, label, committer)
		}
		HashKind::Sha256 => {
			let repo = repo::open_generic::<Sha256>(&found.git_dir, common).await?;
			let (target, label) = plan_checkout::<Sha256>(
				&repo,
				&target,
				commit_ish,
				branch,
				force_branch,
				detach,
				HashKind::Sha256,
			)
			.await?;
			let committer = CliIdentity::new(&repo).committer_or_default().await?;
			(target, label, committer)
		}
	};

	// Delegate all remaining validation + the writes to the library. The effective config supplies
	// `core.logAllRefUpdates` gating (git's full precedence stack), as `list` injects it; `committer` carries
	// the env-aware identity, and `reflog_start` the user's start-point spelling for a new branch's reflog
	// message (git records the token as named — `branch: Created from HEAD` — not the resolved hash).
	let effective = crate::git_config::for_worktree(common, &found.git_dir).await?;
	let reflog_start = matches!(checkout_target, CheckoutTarget::NewBranch { .. })
		.then(|| commit_ish.unwrap_or("HEAD").to_owned());
	let request = CreateRequest {
		repo: RepositoryId::at_common_dir(common.clone())?,
		destination: target.clone(),
		target: checkout_target.clone(),
		committer: Some(committer),
		reflog_start,
	};
	match gitana_linked_worktree::create(&request, Some(&effective)).await {
		// Created, or already exactly present (idempotent) — emit git's "Preparing worktree …" line.
		Ok(_) => {
			report_add(&label, &checkout_target);
			Ok(())
		}
		Err(error) => map_create_error(error, path, &checkout_target),
	}
}

/// The kind of checkout, for the human-facing `add` message.
enum Label {
	/// A branch created for this worktree.
	NewBranch(String),
	/// An existing branch checked out into this worktree.
	CheckoutBranch(String),
	/// A detached `HEAD`.
	Detached,
}

/// Decide the checkout mode from git's DWIM rules, producing the explicit [`CheckoutTarget`] the library
/// takes plus a [`Label`] for the report:
/// - `--detach` → detached `HEAD` at `commit_ish` (default `HEAD`);
/// - `-b`/`-B <name>` → create (or, with `-B` via `force_reset`, reset) branch `<name>` at `commit_ish` —
///   or, in a repo with no commits, orphan it (an unborn branch, empty checkout);
/// - a `commit_ish` that names a local branch → check that branch out; any other → detached `HEAD`;
/// - no `commit_ish` → the basename branch: checked out if it exists, orphaned in an empty repo, else created.
///
/// The branch-exists / branch-in-use / destination checks are the library `create`'s (mapped by the caller);
/// only the DWIM shape and the name/commit-ish resolution (`validate_branch_name`, `resolve_commit`) are here.
async fn plan_checkout<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	target: &Path,
	commit_ish: Option<&str>,
	branch: Option<&str>,
	force_branch: bool,
	detach: bool,
	kind: HashKind,
) -> Result<(CheckoutTarget, Label)> {
	if detach {
		// A detached HEAD needs a concrete commit: an unborn HEAD errors here, as git does.
		let commit = resolve_commit(repo, commit_ish.unwrap_or("HEAD")).await?;
		return Ok((
			CheckoutTarget::Detached {
				start: wt_oid(kind, commit),
			},
			Label::Detached,
		));
	}

	if let Some(name) = branch {
		validate_branch_name(name)?;
		// Strict `-b <name>` where the branch already **exists** refuses "already exists" *before* any
		// use-conflict check — git reports the existence first, even if the branch is also checked out
		// elsewhere. (Only `-B`/`force_branch` proceeds, letting the library refuse an in-use branch as a
		// `BranchUseConflict`; `-B` on a free existing branch resets it.) The library's `decide` checks
		// occupancy before existence, so this precedence is restored CLI-side, matching the old native path.
		if !force_branch
			&& repo
				.refs()
				.resolve(&format!("refs/heads/{name}"))
				.await?
				.is_some()
		{
			bail!("a branch named '{name}' already exists");
		}
		// With no explicit start point in a repository that has no commits at all, git infers an orphan:
		// the new branch is unborn, so there is no commit to check out or point the ref at.
		if commit_ish.is_none() && is_empty_repo(repo).await? {
			return Ok((
				CheckoutTarget::Orphan {
					name: BranchName::new(name),
				},
				Label::NewBranch(name.to_owned()),
			));
		}
		// Otherwise a start point is required: an unborn HEAD in a non-empty repo errors here, as git
		// does (it does not silently orphan onto an existing branch). `-B` sets `force_reset`.
		let commit = resolve_commit(repo, commit_ish.unwrap_or("HEAD")).await?;
		return Ok((
			CheckoutTarget::NewBranch {
				name: BranchName::new(name),
				start: wt_oid(kind, commit),
				force_reset: force_branch,
			},
			Label::NewBranch(name.to_owned()),
		));
	}

	match commit_ish {
		// An explicit start point: check out a branch by that name, otherwise detach at the commit.
		Some(spec) => {
			let refname = format!("refs/heads/{spec}");
			if repo.refs().resolve(&refname).await?.is_some() {
				Ok((
					CheckoutTarget::ExistingBranch {
						name: BranchName::new(spec),
						expected_start: None,
					},
					Label::CheckoutBranch(spec.to_owned()),
				))
			} else {
				let commit = resolve_commit(repo, spec).await?;
				Ok((
					CheckoutTarget::Detached {
						start: wt_oid(kind, commit),
					},
					Label::Detached,
				))
			}
		}
		// No start point: DWIM a branch named after the destination's basename.
		None => {
			let name = target
				.file_name()
				.and_then(|name| name.to_str())
				.ok_or_else(|| anyhow!("invalid worktree path: '{}'", target.display()))?;
			validate_branch_name(name)?;
			let refname = format!("refs/heads/{name}");
			if repo.refs().resolve(&refname).await?.is_some() {
				Ok((
					CheckoutTarget::ExistingBranch {
						name: BranchName::new(name),
						expected_start: None,
					},
					Label::CheckoutBranch(name.to_owned()),
				))
			} else if is_empty_repo(repo).await? {
				// A repo with no commits at all: orphan the new unborn branch (empty checkout).
				Ok((
					CheckoutTarget::Orphan {
						name: BranchName::new(name),
					},
					Label::NewBranch(name.to_owned()),
				))
			} else {
				let commit = resolve_commit(repo, "HEAD").await?;
				Ok((
					CheckoutTarget::NewBranch {
						name: BranchName::new(name),
						start: wt_oid(kind, commit),
						force_reset: false,
					},
					Label::NewBranch(name.to_owned()),
				))
			}
		}
	}
}

/// Tag a resolved `ObjectId<H>` with its runtime hash kind for the library boundary — a resolved id is
/// always valid hex for its own kind, so the parse cannot fail.
fn wt_oid<H: HashAlgorithm>(kind: HashKind, id: ObjectId<H>) -> WorktreeObjectId {
	WorktreeObjectId::parse(kind, &id.to_hex())
		.expect("a resolved object id is valid hex for its own kind")
}

/// Resolve `spec` to a commit, peeling an (annotated) tag to the commit it names, as git accepts any
/// commit-ish start point.
async fn resolve_commit<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	spec: &str,
) -> Result<ObjectId<H>> {
	Ok(repo.peel_to_commit(repo.rev_parse(spec).await?).await?)
}

/// Whether the repository holds no commit at all — an unborn `HEAD` *and* no existing branch — so a
/// new worktree with no start point must be an orphan. git infers `--orphan` only in this case; an
/// unborn `HEAD` in a repository that already has branches is an error, not an orphan.
async fn is_empty_repo<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<bool> {
	Ok(
		repo.refs().resolve_head().await?.is_none()
			&& repo.refs().list("refs/heads/").await?.is_empty(),
	)
}

/// Reject a branch name git's `check-ref-format --branch` would reject, before any ref or admin
/// layout is written (a path-derived DWIM name or an explicit `-b`/`-B` name). Otherwise a name like
/// `wt space` would be written as the broken ref `refs/heads/wt space`, which stock git then rejects.
fn validate_branch_name(name: &str) -> Result<()> {
	let bad = name.is_empty()
		// `HEAD` is reserved (git `check-ref-format --branch` rejects it); `@` alone is permitted.
		|| name == "HEAD"
		|| name.starts_with('-')
		|| name.starts_with('/')
		|| name.ends_with('/')
		|| name.ends_with('.')
		|| name.contains("..")
		|| name.contains("//")
		|| name.contains("@{")
		|| name
			.chars()
			.any(|c| c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
		// Each `/`-separated component: non-empty, not `.`-led, not `.lock`-tailed.
		|| name
			.split('/')
			.any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"));
	if bad {
		bail!("'{name}' is not a valid branch name");
	}
	Ok(())
}

/// Emit git's `Preparing worktree …` stderr line for a successful add, from the resolved
/// [`CheckoutTarget`] (for the detached-HEAD short id).
fn report_add(label: &Label, target: &CheckoutTarget) {
	match label {
		Label::NewBranch(name) => eprintln!("Preparing worktree (new branch '{name}')"),
		Label::CheckoutBranch(name) => eprintln!("Preparing worktree (checking out '{name}')"),
		Label::Detached => {
			let hex = match target {
				CheckoutTarget::Detached { start } => start.to_hex(),
				_ => unreachable!("a Detached label always pairs with a Detached checkout target"),
			};
			eprintln!(
				"Preparing worktree (detached HEAD {})",
				&hex[..7.min(hex.len())]
			);
		}
	}
}

/// The short branch name a checkout target carries (`None` for a detached target) — used to name the
/// branch in a `BranchUseConflict` refusal message.
fn target_branch_short(target: &CheckoutTarget) -> Option<&str> {
	match target {
		CheckoutTarget::NewBranch { name, .. }
		| CheckoutTarget::ExistingBranch { name, .. }
		| CheckoutTarget::Orphan { name } => Some(name.short()),
		CheckoutTarget::Detached { .. } => None,
	}
}

/// Map a library [`CreateError`] onto git's `worktree add` messages (the oracle pins several substrings).
fn map_create_error(error: CreateError, path: &Path, target: &CheckoutTarget) -> Result<()> {
	use WorktreeClassification as C;
	match error {
		// A non-empty/foreign destination already occupies the path (git: "already exists").
		CreateError::Refused(C::DestinationConflict { .. })
		| CreateError::ExistingWorktreeMismatch(_) => bail!("'{}' already exists", path.display()),
		// The checkout is gone but the registration remains — a stale, still-registered path.
		CreateError::Refused(C::PartialRegistered { .. }) => bail!(
			"'{}' is a missing but already registered worktree",
			path.display()
		),
		// The requested branch is checked out in another worktree — git refuses a second checkout.
		CreateError::Refused(C::BranchUseConflict { other_checkout }) => bail!(
			"'{}' is already checked out at '{}'",
			target_branch_short(target).unwrap_or("HEAD"),
			other_checkout.display()
		),
		// A strict `-b` (or orphan) whose branch already exists.
		CreateError::BranchExists(name) => bail!("a branch named '{name}' already exists"),
		// An `ExistingBranch` whose ref vanished (a race) — git says "invalid reference".
		CreateError::BranchNotFound(name) => bail!("invalid reference: {name}"),
		// Defensive: `plan_checkout`'s `validate_branch_name` catches this first.
		CreateError::InvalidBranchName(name) => bail!("'{name}' is not a valid branch name"),
		CreateError::UnsupportedSymbolicBranchReset(name) => {
			bail!("cannot reset symbolic-ref branch '{name}'; reset its terminal branch directly")
		}
		// A checkout present but unregistered / cross-pointer-inconsistent occupies the destination.
		CreateError::Refused(C::PartialConflicting { .. } | C::IdentityConflict { .. }) => {
			bail!(
				"'{}' contains a conflicting worktree checkout",
				path.display()
			)
		}
		CreateError::Refused(C::ProtectedWithReason {
			reason: ProtectionReason::Locked { .. },
		}) => bail!("destination worktree is locked"),
		CreateError::NotEstablished(_) => {
			bail!("worktree add did not complete; re-inspect and re-run")
		}
		CreateError::Failed(error) => Err(error.into()),
		// Any other refusal classification (not expected from a create) — a clear catch-all.
		CreateError::Refused(_) => bail!("cannot add a worktree at '{}'", path.display()),
	}
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(cwd: &Path, porcelain: bool) -> Result<()> {
	let found = repo::discover(cwd).await?;
	// Resolve the *invoking* worktree's effective config (git's full precedence stack) here, where the
	// discovered layout still carries that worktree's git dir. The library honours the injected
	// `core.ignorecase` for its listing order (git sorts linked worktrees by checkout path, case-folded
	// when `core.ignorecase` is set — typical on macOS/Windows).
	let effective = crate::git_config::for_worktree(&found.common_dir, &found.git_dir).await?;
	// `core.ignorecase` is a startup `core.*` boolean: git validates every occurrence and aborts on any
	// malformed value — even one shadowed by a higher-precedence source. The library trusts its injected
	// config (validation is a property of a git process booting, not of a library answering a query), so
	// keep git's abort here at the CLI edge, as `list` has always done.
	effective.get_bool_validated("core", None, "ignorecase")?;

	// Delegating to the library closes a symlink disclosure class the native collector inherited from git:
	// git follows a symlinked `worktrees/` container, a symlinked admin leaf, and a symlinked `locked`
	// marker — printing the marker target's contents as the lock reason. The library never reads through
	// those links, so this listing diverges from git by *skipping* a worktree reached only via a symlinked
	// container/leaf, and by *withholding* a symlinked lock reason (see `tests/git_worktree.rs`).
	let cx = WorktreeContext::with_effective_config(
		RepositoryId::at_common_dir(found.common_dir.clone())?,
		effective,
	);
	let listing = gitana_linked_worktree::enumerate(&cx).await?;
	// The library reports no object for an unborn HEAD; `kind` renders its all-zeros placeholder at the
	// repository's hash width.
	let kind = detect_algorithm(&found.common_dir)?;
	let entries: Vec<WorktreeInfo> = listing
		.entries
		.into_iter()
		.map(|entry| info_from_entry(entry, kind))
		.collect();
	if porcelain {
		print!("{}", render_porcelain(&entries));
	} else {
		print!("{}", render_default(&entries));
	}
	Ok(())
}

/// One row of `worktree list`: the checkout's path, its state, and its lock/prune attributes.
struct WorktreeInfo {
	path: PathBuf,
	state: State,
	/// The lock reason if the worktree is locked (an empty string when locked without a reason).
	locked: Option<String>,
	/// The prune reason if the worktree's checkout is gone (git's stale-entry marker).
	prunable: Option<String>,
}

/// A worktree's state for listing: a bare repository (no checkout), or a checkout with its `HEAD`
/// commit (all-zeros hex when the branch is unborn) and its branch (`None` for a detached `HEAD`).
enum State {
	Bare,
	Checkout {
		head: String,
		branch: Option<String>,
	},
}

/// Map a library [`WorktreeEntry`] onto the row [`render_default`]/[`render_porcelain`] emit. `kind` is
/// the repository's hash algorithm, used only to render the all-zeros `HEAD` of an unborn branch at the
/// right width (the library reports no object for it). git derives a row's state from its resolved object
/// and branch: a detached HEAD has an object but no branch, an unborn one a branch but no object.
fn info_from_entry(entry: WorktreeEntry, kind: HashKind) -> WorktreeInfo {
	let locked = match entry.lock {
		LockState::Unlocked => None,
		// A symlinked `locked` marker resolves to `Locked { reason: None }` — the library withholds the
		// target's contents rather than disclosing them as git does — so render it as a reasonless lock.
		LockState::Locked { reason } => Some(reason.unwrap_or_default()),
	};
	let state = match entry.role {
		WorktreeRole::Primary { bare: true } => State::Bare,
		_ => State::Checkout {
			head: entry
				.object
				.map(|object| object.to_hex())
				.unwrap_or_else(|| zero_hex(kind)),
			branch: entry.branch,
		},
	};
	// A stale worktree (its checkout gone) is prunable — unless it is locked, since the lock protects it
	// (git then reports only `locked`, not `prunable`).
	let prunable = (entry.checkout_missing && locked.is_none())
		.then(|| "gitdir file points to non-existent location".to_owned());
	WorktreeInfo {
		path: entry.path,
		state,
		locked,
		prunable,
	}
}

/// The lock reason for a worktree whose git directory holds a `locked` file — `Some("")` when locked
/// without a reason, `None` when unlocked. git writes the reason (if any) as the file's contents.
fn read_lock_reason(git_dir: &Path) -> Option<String> {
	match std::fs::read_to_string(git_dir.join("locked")) {
		Ok(reason) => Some(reason.trim().to_owned()),
		Err(_) => None,
	}
}

/// The all-zeros object-id hex for the repository's hash algorithm (`2 * RAW_LEN` zeros), git's
/// placeholder for an unborn `HEAD`.
fn zero_hex(kind: HashKind) -> String {
	let raw_len = match kind {
		HashKind::Sha1 => Sha1::RAW_LEN,
		HashKind::Sha256 => Sha256::RAW_LEN,
	};
	"0".repeat(raw_len * 2)
}

/// `git worktree list` default form: `<path>  <short-oid> <marker>`, path column padded to align; a
/// bare repository renders as `<path>  (bare)`.
fn render_default(entries: &[WorktreeInfo]) -> String {
	// git pads the path column to the longest path plus one, then a single space before the oid.
	let width = entries
		.iter()
		.map(|entry| entry.path.to_string_lossy().len())
		.max()
		.unwrap_or(0)
		+ 1;
	let mut out = String::new();
	for entry in entries {
		let path = entry.path.to_string_lossy();
		let mut line = match &entry.state {
			State::Bare => format!("{path:<width$} (bare)"),
			State::Checkout { head, branch } => {
				let marker = match branch {
					Some(refname) => {
						format!(
							"[{}]",
							refname.strip_prefix("refs/heads/").unwrap_or(refname)
						)
					}
					None => "(detached HEAD)".to_owned(),
				};
				let short = &head[..7.min(head.len())];
				format!("{path:<width$} {short} {marker}")
			}
		};
		// git appends `locked`/`prunable` markers (the default form shows no lock reason).
		if entry.locked.is_some() {
			line.push_str(" locked");
		}
		if entry.prunable.is_some() {
			line.push_str(" prunable");
		}
		out.push_str(&line);
		out.push('\n');
	}
	out
}

/// `git worktree list --porcelain` form: a `worktree` block per worktree — `bare`, or `HEAD` plus
/// `branch`/`detached` — separated (and terminated) by a blank line.
fn render_porcelain(entries: &[WorktreeInfo]) -> String {
	let mut out = String::new();
	for entry in entries {
		out.push_str(&format!("worktree {}\n", entry.path.to_string_lossy()));
		match &entry.state {
			State::Bare => out.push_str("bare\n"),
			State::Checkout { head, branch } => {
				out.push_str(&format!("HEAD {head}\n"));
				match branch {
					Some(refname) => out.push_str(&format!("branch {refname}\n")),
					None => out.push_str("detached\n"),
				}
			}
		}
		// git emits `locked [<reason>]` then `prunable <reason>` before the blank separator.
		if let Some(reason) = &entry.locked {
			if reason.is_empty() {
				out.push_str("locked\n");
			} else {
				out.push_str(&format!("locked {reason}\n"));
			}
		}
		if let Some(reason) = &entry.prunable {
			out.push_str(&format!("prunable {reason}\n"));
		}
		out.push('\n');
	}
	out
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

async fn remove(cwd: &Path, path: &Path, force: u8) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let common = &found.common_dir;
	// Resolve by git's rules (exact path, then a unique name/id suffix) — kept CLI-side (the DWIM the library
	// does not do). The checkout may already be gone (deleted or moved) — git still cleans up such a stale
	// entry — so `find_worktree` matches on the recorded path without requiring it to exist.
	let (admin, target) = match find_worktree(common, cwd, path) {
		Some(WorktreeRef::Main { .. }) => bail!("'{}' is a main working tree", path.display()),
		Some(WorktreeRef::Linked { admin, path }) => (admin, path),
		None => bail!("'{}' is not a working tree", path.display()),
	};

	// Submodule guard stays CLI-side: the library has no submodule concept, and git refuses (without `--force`)
	// to remove a worktree holding an initialized submodule (deleting it would orphan the submodule's git data).
	// A single `--force` overrides it, matching git.
	if force < 1 && target.exists() && worktree_has_submodule(&admin, &target) {
		bail!("working trees containing submodules cannot be moved or removed");
	}

	// Delegate the removal to the library (`GitCompat` = git's repeatable-`-f` semantics): it owns the
	// lock / dirty / structural-validation / stale-registration-cleanup decisions and the destructive effect
	// (with a pre-delete re-verify). The CLI maps the structured outcome to git's messages.
	let request = RemoveRequest {
		repo: RepositoryId::at_common_dir(common.clone())?,
		destination: target.clone(),
		expected_branch: None,
		policy: RemovePolicy::GitCompat { force },
	};
	use ProtectionReason as P;
	use WorktreeClassification as C;
	match gitana_linked_worktree::remove(&request).await {
		// Removed, or already gone (idempotent) — git prints nothing on a successful remove.
		Ok(RemoveOutcome::Removed { .. } | RemoveOutcome::AlreadyAbsent { .. }) => Ok(()),
		// A locked worktree needs a second `-f`; carry git's lock-reason message when one is recorded.
		Err(RemoveError::Refused(C::ProtectedWithReason {
			reason: P::Locked { reason },
		})) => match reason {
			Some(r) if !r.is_empty() => {
				bail!("cannot remove a locked working tree, lock reason: {r}")
			}
			_ => bail!("cannot remove a locked working tree"),
		},
		// Any modified/untracked/ignored/staged residue (without enough force) is git's "modified or untracked".
		Err(RemoveError::Refused(C::ProtectedWithReason {
			reason:
				P::Dirty(_)
				| P::ResidualContent { .. }
				| P::ModifiedTrackedContent { .. }
				| P::StagedContentInMissingCheckout,
		})) => bail!(
			"'{}' contains modified or untracked files, use --force to delete it",
			path.display()
		),
		// A broken/foreign/reused checkout or a registration conflict is git's structural "validation failed".
		Err(RemoveError::Refused(
			C::DestinationConflict { .. } | C::IdentityConflict { .. } | C::PartialConflicting { .. },
		)) => bail!(
			"validation failed, cannot remove working tree: '{}' is not a valid working tree",
			target.join(".git").display()
		),
		// A sparse-index checkout gitana cannot safely status (conservative-stricter than git force-0 by design).
		Err(RemoveError::Refused(C::ProtectedWithReason {
			reason: P::SparseIndexUnsupported,
		})) => bail!(
			"'{}' uses a sparse-checkout index gitana cannot remove; remove it with git",
			path.display()
		),
		// Removing would orphan a commit anchored only in the admin dir (conservative-stricter than git).
		Err(RemoveError::Refused(C::ProtectedWithReason {
			reason: P::UnreachableAnchoredCommit { commit },
		})) => bail!(
			"refusing to remove '{}': it would orphan commit {}; create a branch or tag at it first",
			path.display(),
			commit.to_hex()
		),
		// Defensive: `find_worktree` catches the main worktree first, but honour a library primary refusal too.
		Err(RemoveError::IsPrimaryWorktree(_)) => bail!("'{}' is a main working tree", path.display()),
		Err(RemoveError::EnclosesRepository(p)) => bail!(
			"cannot remove a worktree enclosing the repository ({})",
			p.display()
		),
		Err(RemoveError::Incomplete(_)) => bail!(
			"remove did not complete; re-inspect '{}' and re-run",
			path.display()
		),
		Err(RemoveError::Failed(e)) => Err(e.into()),
		// Any other refusal classification (not expected from a remove) maps to git's validation message.
		Err(RemoveError::Refused(_)) => bail!(
			"validation failed, cannot remove working tree: '{}' is not a valid working tree",
			target.join(".git").display()
		),
	}
}

/// The admin directory under `<common>/worktrees/*` whose checkout is `target`, or `None` if no
/// linked worktree lives there.
fn admin_dir_for(common: &Path, target: &Path) -> Option<PathBuf> {
	let entries = std::fs::read_dir(common.join("worktrees")).ok()?;
	entries
		.flatten()
		.map(|entry| entry.path())
		.find(|admin| canonical_eq(&repo::worktree_path_of(admin), target))
}

/// Whether the checkout's `.git` file (`gitfile`) points back at `admin` (`gitdir: <admin>`) — the
/// link that proves the directory at the recorded path is really this worktree, not an unrelated one.
/// git may write a relative pointer (`worktree.useRelativePaths`), resolved against the checkout dir.
fn checkout_points_to(gitfile: &Path, admin: &Path) -> bool {
	std::fs::read_to_string(gitfile)
		.ok()
		.and_then(|content| {
			content
				.lines()
				.next()
				.and_then(|line| line.strip_prefix("gitdir:"))
				.map(|dir| {
					let pointer = Path::new(dir.trim());
					let resolved = if pointer.is_absolute() {
						pointer.to_path_buf()
					} else {
						gitfile.parent().unwrap_or(Path::new(".")).join(pointer)
					};
					canonical_eq(&resolved, admin)
				})
		})
		.unwrap_or(false)
}

// ---------------------------------------------------------------------------
// lock / unlock
// ---------------------------------------------------------------------------

/// Lock the worktree named by `arg` (git `worktree lock`): write `<admin>/locked`, holding the reason
/// if one is given. A locked worktree resists `prune` and needs a second `-f` to `remove`.
async fn lock(cwd: &Path, arg: &Path, reason: Option<&str>) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let admin = resolve_lockable(&found.common_dir, cwd, arg)?;
	// git refuses to re-lock an already-locked worktree, echoing the existing reason.
	if let Some(existing) = read_lock_reason(&admin) {
		if existing.is_empty() {
			bail!("'{}' is already locked", arg.display());
		}
		bail!("'{}' is already locked, reason: {existing}", arg.display());
	}
	// git writes the reason followed by a newline, or an empty file when locked without a reason.
	let body = match reason {
		Some(reason) if !reason.is_empty() => format!("{reason}\n"),
		_ => String::new(),
	};
	std::fs::write(admin.join("locked"), body)
		.map_err(|error| anyhow!("locking {}: {error}", arg.display()))?;
	Ok(())
}

/// Unlock the worktree named by `arg` (git `worktree unlock`): remove `<admin>/locked`.
async fn unlock(cwd: &Path, arg: &Path) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let admin = resolve_lockable(&found.common_dir, cwd, arg)?;
	if read_lock_reason(&admin).is_none() {
		bail!("'{}' is not locked", arg.display());
	}
	std::fs::remove_file(admin.join("locked"))
		.map_err(|error| anyhow!("unlocking {}: {error}", arg.display()))?;
	Ok(())
}

/// Resolve `arg` to a *linked* worktree's admin directory for lock/unlock, rejecting the main
/// worktree (git: "The main working tree cannot be locked or unlocked") and an unknown worktree
/// (git: "'<arg>' is not a working tree").
fn resolve_lockable(common: &Path, cwd: &Path, arg: &Path) -> Result<PathBuf> {
	match find_worktree(common, cwd, arg) {
		Some(WorktreeRef::Linked { admin, .. }) => Ok(admin),
		Some(WorktreeRef::Main { .. }) => {
			bail!("The main working tree cannot be locked or unlocked")
		}
		None => bail!("'{}' is not a working tree", arg.display()),
	}
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// Prune the admin directories of worktrees whose checkout has gone (git `worktree prune`). Mirrors
/// git's `should_prune_worktree`: a locked worktree is kept; a missing/empty `gitdir` file, or a
/// `gitdir` whose target `.git` file is gone, marks the entry stale. When stale, `--expire` keeps it
/// if its per-worktree `index` is newer than the cutoff (git compares the `index` mtime). `dry_run`
/// (`-n`) reports without removing; each removal is reported to stderr when `dry_run` or `verbose`.
async fn prune(cwd: &Path, dry_run: bool, verbose: bool, expire: Option<&str>) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let common = &found.common_dir;
	// Default (no `--expire`): remove every stale worktree — git uses an effectively-infinite cutoff.
	let cutoff = match expire {
		Some(spec) => parse_expiry(spec)?,
		None => u64::MAX,
	};
	let worktrees = common.join("worktrees");
	let mut names: Vec<String> = match std::fs::read_dir(&worktrees) {
		Ok(entries) => entries
			.flatten()
			.filter_map(|entry| entry.file_name().into_string().ok())
			.collect(),
		Err(_) => return Ok(()),
	};
	// A stable order keeps the (stderr) report deterministic; git walks readdir order.
	names.sort();
	for name in names {
		let admin = worktrees.join(&name);
		let Some(reason) = prune_reason(&admin, cutoff) else {
			continue;
		};
		if dry_run || verbose {
			eprintln!("Removing worktrees/{name}: {reason}");
		}
		if !dry_run {
			// A malformed entry that is a plain file (`not a valid directory`) must be unlinked, not
			// `remove_dir_all`-ed — prune is the cleanup path for exactly such corrupt admin entries.
			let removed = if admin.is_dir() {
				std::fs::remove_dir_all(&admin)
			} else {
				std::fs::remove_file(&admin)
			};
			removed.map_err(|error| anyhow!("removing {}: {error}", admin.display()))?;
		}
	}
	Ok(())
}

/// The reason to prune the admin directory `admin`, or `None` to keep it — git's
/// `should_prune_worktree`:
/// - not a directory → "not a valid directory";
/// - a `locked` file present → keep (a lock protects a stale worktree from pruning);
/// - `gitdir` file missing → "gitdir file does not exist";
/// - `gitdir` file empty → "invalid gitdir file";
/// - the `.git` file it points at is gone → "gitdir file points to non-existent location", *unless*
///   the per-worktree `index` is newer than `cutoff` (then keep — the worktree was used recently).
fn prune_reason(admin: &Path, cutoff: u64) -> Option<String> {
	if !admin.is_dir() {
		return Some("not a valid directory".to_owned());
	}
	if admin.join("locked").exists() {
		return None;
	}
	let pointer = match std::fs::read_to_string(admin.join("gitdir")) {
		Ok(text) => text,
		Err(_) => return Some("gitdir file does not exist".to_owned()),
	};
	let pointer = pointer.trim();
	if pointer.is_empty() {
		return Some("invalid gitdir file".to_owned());
	}
	// `gitdir` records the checkout's `.git` file; a relative pointer resolves against the admin dir.
	let git_file = {
		let pointer = Path::new(pointer);
		if pointer.is_absolute() {
			pointer.to_path_buf()
		} else {
			admin.join(pointer)
		}
	};
	if git_file.exists() {
		return None;
	}
	// Stale: keep it only when `--expire` leaves the per-worktree index newer than the cutoff. A
	// missing index (an orphan worktree never checked out, or one already cleaned) is always prunable.
	if index_mtime_secs(admin).is_some_and(|mtime| mtime > cutoff) {
		return None;
	}
	Some("gitdir file points to non-existent location".to_owned())
}

/// The mtime (seconds since the Unix epoch) of the worktree's per-worktree `index`, or `None` when it
/// has none. git compares this against `--expire`.
fn index_mtime_secs(admin: &Path) -> Option<u64> {
	let mtime = std::fs::metadata(admin.join("index"))
		.ok()?
		.modified()
		.ok()?;
	Some(mtime.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// A bare integer is accepted as a `--expire` value only when it is large enough to be an unambiguous
/// Unix timestamp (roughly 1973 onward). git's `approxidate` treats *small* integers as fuzzy date
/// components rather than epoch seconds — and does so non-monotonically (`0` and `100` prune, `1`
/// keeps) — which gta deliberately does not reproduce; a small integer is rejected with a clear error
/// rather than silently mis-dated as literal epoch seconds (which would behave like `never`).
const MIN_EPOCH_EXPIRY: u64 = 100_000_000;

/// Parse a git `--expire` time into seconds since the Unix epoch. Supports the forms gta needs: a
/// Unix timestamp (a large integer — git's approxidate also reads one as epoch seconds), `now`, `all`,
/// `never`, and simple relative spans (`2.weeks.ago`, `3 days ago`, `1.year`). git's full approxidate
/// grammar (absolute calendar dates, `yesterday`, small fuzzy integers, …) is intentionally not
/// reproduced — a clear error is returned rather than silently mis-dating a prune.
fn parse_expiry(spec: &str) -> Result<u64> {
	let spec = spec.trim();
	match spec {
		// git's `parse_expiry_date` special-cases these: `now`/`all` mean "expire everything" (an
		// effectively infinite cutoff — *not* the current time, so a future-dated index is still
		// pruned), and `never`/`false` mean "do not expire by age" (a zero cutoff is older than any
		// real mtime, so a stale worktree with an index is kept).
		"now" | "all" => return Ok(u64::MAX),
		"never" | "false" => return Ok(0),
		_ => {}
	}
	if let Ok(secs) = spec.parse::<u64>()
		&& secs >= MIN_EPOCH_EXPIRY
	{
		return Ok(secs);
	}
	if let Some(span) = parse_relative_span(spec) {
		return Ok(now_secs()?.saturating_sub(span));
	}
	bail!(
		"unsupported expiry time: '{spec}' (use a Unix timestamp, 'now', 'never', or e.g. '2.weeks.ago')"
	)
}

/// The current time in seconds since the Unix epoch.
fn now_secs() -> Result<u64> {
	Ok(
		SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map_err(|error| anyhow!("system clock is before the Unix epoch: {error}"))?
			.as_secs(),
	)
}

/// Parse a relative span like `2.weeks.ago`, `3 days`, or `1.year.ago` into a number of seconds, or
/// `None` if it isn't a span gta recognises. Approximate (a month is 30 days, a year 365) — precise
/// enough for a prune cutoff, as git's own approxidate is for these forms.
fn parse_relative_span(spec: &str) -> Option<u64> {
	// Both `.`-separated (`2.weeks.ago`) and space-separated (`2 weeks ago`) forms tokenise the same.
	let normalised = spec.replace('.', " ");
	let mut parts = normalised.split_whitespace();
	let count: u64 = parts.next()?.parse().ok()?;
	let unit = parts.next()?;
	// An optional trailing `ago` is the only extra token allowed.
	match parts.next() {
		Some("ago") if parts.next().is_none() => {}
		Some(_) => return None,
		None => {}
	}
	let unit = unit.strip_suffix('s').unwrap_or(unit);
	let factor: u64 = match unit {
		"second" | "sec" => 1,
		"minute" | "min" => 60,
		"hour" => 3_600,
		"day" => 86_400,
		"week" => 604_800,
		"month" => 2_592_000, // 30 days
		"year" => 31_536_000, // 365 days
		_ => return None,
	};
	Some(count.saturating_mul(factor))
}

// ---------------------------------------------------------------------------
// move
// ---------------------------------------------------------------------------

/// Relocate a linked worktree's checkout to `new_path` (git `worktree move`): move the directory, then
/// repoint the admin's `gitdir` backlink at the checkout's new `.git` file. `force` is git's repeatable
/// flag — two forces move a locked worktree, one moves onto a path still registered to a since-deleted
/// worktree. git's destination rule matches `mv`: when `new_path` is an existing directory the checkout
/// moves *into* it under its own basename, otherwise `new_path` is the literal target.
async fn move_worktree(cwd: &Path, worktree: &Path, new_path: &Path, force: u8) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let common = &found.common_dir;
	let (admin, source) = match find_worktree(common, cwd, worktree) {
		Some(WorktreeRef::Main { .. }) => bail!("'{}' is a main working tree", worktree.display()),
		Some(WorktreeRef::Linked { admin, path }) => (admin, path),
		None => bail!("'{}' is not a working tree", worktree.display()),
	};

	// The checkout must be present and genuinely this worktree's before we relocate it (git's
	// `validate_worktree`): its `.git` file must exist and point back at this admin directory. A stale or
	// foreign checkout is refused rather than moved.
	let gitfile = source.join(".git");
	if !gitfile.is_file() {
		bail!(
			"validation failed, cannot move working tree: '{}' does not exist",
			gitfile.display()
		);
	}
	if !checkout_points_to(&gitfile, &admin) {
		bail!(
			"validation failed, cannot move working tree: '{}' is not a .git file",
			gitfile.display()
		);
	}

	// gitana has no submodule support; git refuses to move a worktree holding an initialized submodule
	// (the submodule's own `.git` link would be left dangling at the old path), so gitana refuses too
	// rather than corrupt a git-created one. An *uninitialized* submodule is no obstacle, as git allows.
	if worktree_has_submodule(&admin, &source) {
		bail!("working trees containing submodules cannot be moved or removed");
	}

	// A locked worktree needs two `-f` to move (one is not enough), echoing the reason as git does.
	if let Some(reason) = read_lock_reason(&admin)
		&& force < 2
	{
		if reason.is_empty() {
			bail!("cannot move a locked working tree;\nuse 'move -f -f' to override or unlock first");
		}
		bail!(
			"cannot move a locked working tree, lock reason: {reason}\nuse 'move -f -f' to override or unlock first"
		);
	}

	// The destination, computed git's way: into an existing directory under the source's basename, else
	// the literal path. `display` mirrors what git prints (the argument, with the basename appended when
	// moving into a directory).
	let base = source
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| {
			anyhow!(
				"could not figure out destination name from '{}'",
				source.display()
			)
		})?;
	let raw = absolute(cwd, new_path);
	let (dest, display) = if raw.is_dir() {
		let arg = new_path.to_string_lossy();
		(
			raw.join(base),
			format!("{}/{base}", arg.trim_end_matches('/')),
		)
	} else {
		(raw, new_path.to_string_lossy().into_owned())
	};

	// An occupied destination (a non-empty directory or a file) is refused; an empty directory is fine,
	// as git moves onto one.
	if dest.exists() && dir_non_empty(&dest)? {
		bail!("'{display}' already exists");
	}
	// A destination still registered to another — since-deleted — worktree needs a force; with it, that
	// stale admin entry is dropped (as git does) so the repository never ends up with two admin dirs for
	// one checkout path. A *locked* stale registration is protected: like `remove`, it takes a second
	// `-f` (git refuses one), so a lock set to prevent cleanup is not discarded by a single force.
	let stale_registration =
		admin_dir_for(common, &canonical(&dest)).filter(|other| !canonical_eq(other, &admin));
	if let Some(other) = &stale_registration {
		if read_lock_reason(other).is_some() {
			if force < 2 {
				bail!(
					"'{display}' is a missing but locked worktree;\nuse 'move -f -f' to override, or 'unlock' and 'prune' or 'remove' to clear"
				);
			}
		} else if force < 1 {
			bail!(
				"'{display}' is a missing but already registered worktree;\nuse 'move -f' to override, or 'prune' or 'remove' to clear"
			);
		}
	}

	// Preserve each pointer's representation across the move: a git `worktree.useRelativePaths` worktree
	// records relative pointers so the tree can be relocated as a unit, and forcing them to absolute would
	// defeat that. Capture whether each side is relative *before* the rename (source still in place).
	let checkout_relative = gitfile_is_relative(&gitfile);
	let admin_relative = admin_gitdir_is_relative(&admin.join("gitdir"));

	// An empty directory left at the destination would block `rename`; clear it first (validated empty).
	if dest.is_dir() {
		std::fs::remove_dir(&dest).map_err(|error| anyhow!("removing {}: {error}", dest.display()))?;
	}
	std::fs::rename(&source, &dest).map_err(|error| {
		anyhow!(
			"failed to move '{}' to '{}': {error}",
			source.display(),
			new_path.display()
		)
	})?;
	// Drop the stale destination registration only *after* the move succeeds, so a failed rename leaves
	// that (recoverable) admin entry intact rather than discarding it, as git does.
	if let Some(other) = &stale_registration {
		std::fs::remove_dir_all(other)
			.map_err(|error| anyhow!("removing {}: {error}", other.display()))?;
	}

	// Repoint the admin's backlink at the checkout's new `.git` file, and — only when the checkout used a
	// relative pointer, now wrong at the new depth — rewrite the checkout's own `.git` file too. An
	// absolute checkout pointer moved with the directory and still names the (unmoved) admin directory.
	let git_file = dest.join(".git");
	let backlink = admin.join("gitdir");
	std::fs::write(
		&backlink,
		format!("{}\n", pointer(&admin, &git_file, admin_relative)),
	)
	.map_err(|error| anyhow!("updating {}: {error}", backlink.display()))?;
	if checkout_relative {
		std::fs::write(
			&git_file,
			format!("gitdir: {}\n", pointer(&dest, &admin, true)),
		)
		.map_err(|error| anyhow!("updating {}: {error}", git_file.display()))?;
	}
	Ok(())
}

// ---------------------------------------------------------------------------
// repair
// ---------------------------------------------------------------------------

/// Repair the cross-pointers between linked checkouts and their admin directories after a manual move
/// (git `worktree repair`), reconciling both directions. Two passes, each reconciling a worktree's two
/// pointers: first each given checkout path (default the current worktree) with the admin it names, then
/// every registered admin under `<common>/worktrees/*` with its recorded checkout. Each correction is
/// reported to stderr; a healthy link is silent.
///
/// The admin directory is the stable anchor — it lives under the common dir, which discovery resolves
/// reliably — so even a checkout whose relative `.git` pointer is stale at a new depth is matched by the
/// admin *name* (the pointer's final component) and fully repaired, both pointers, as git does.
async fn repair(cwd: &Path, paths: &[PathBuf]) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let common = &found.common_dir;

	// Pass 1 — reconcile each given checkout with the admin its `.git` file names. The no-arg default is
	// the discovered worktree *root* (not the raw cwd, which may be a subdirectory of a moved checkout).
	if paths.is_empty() {
		// `work` is `None` only in a bare repo, which has no linked checkout to repair from here.
		if let Some(root) = &found.worktree_root
			&& let Some(admin) = admin_for_checkout(common, root)
		{
			reconcile(&admin, root)?;
		}
	} else {
		for path in paths {
			let checkout = absolute(cwd, path);
			// git errors on a repair target that is not a worktree rather than silently succeeding: a
			// non-existent path, or an existing one with no readable `.git` file naming a known admin.
			if !checkout.exists() {
				bail!("not a valid path: {}", path.display());
			}
			// The main worktree (its `.git` is a directory) is a valid explicit target with no backlink of
			// its own — git accepts it and leaves the linked-checkout repairs to pass 2.
			if checkout.join(".git").is_dir() {
				continue;
			}
			// Find the admin from the checkout's own `.git` pointer, or — when that file is missing or
			// garbage — from the admin that still registers this checkout path, so an explicitly named
			// checkout with a broken `.git` file is recreated (as git does) rather than rejected.
			let admin = admin_for_checkout(common, &checkout)
				.or_else(|| admin_dir_for(common, &canonical(&checkout)));
			let Some(admin) = admin else {
				bail!(
					"unable to locate repository; .git file broken: {}",
					canonical(&checkout).join(".git").display()
				);
			};
			reconcile(&admin, &checkout)?;
		}
	}

	// Pass 2 — reconcile every registered admin with its recorded checkout (present on disk; git leaves a
	// missing one to `prune`). Pointers pass 1 already fixed re-verify as healthy here, so nothing is
	// reported twice.
	if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
		let mut admins: Vec<PathBuf> = entries
			.flatten()
			.map(|entry| entry.path())
			.filter(|admin| admin.join("gitdir").is_file())
			.collect();
		admins.sort();
		for admin in admins {
			if let Some(checkout) = checkout_for_admin(&admin)
				&& checkout.is_dir()
			{
				reconcile(&admin, &checkout)?;
			}
		}
	}
	Ok(())
}

/// Reconcile the two cross-pointers of one linked worktree: `<admin>/gitdir` must name the checkout's
/// `.git` file, and `<checkout>/.git` must name the admin directory. Each side is rewritten — preserving
/// its absolute/relative representation for the new location — only when it does not already resolve
/// correctly, reporting the correction to stderr (the admin backlink first, as git orders them). A write
/// failure is surfaced as an error rather than a false "repaired", so the worktree is not left broken.
fn reconcile(admin: &Path, checkout: &Path) -> Result<()> {
	let dotgit = checkout.join(".git");
	let backlink = admin.join("gitdir");

	// Both pointers share the worktree's absolute/relative choice (`worktree.useRelativePaths`); when the
	// side being rewritten is itself missing, infer its form from the surviving side so a recreated
	// pointer stays relative (keeping the tree relocatable as a unit), as git does.
	let relative = admin_gitdir_is_relative(&backlink) || gitfile_is_relative(&dotgit);

	// `<admin>/gitdir` → the checkout's `.git` file.
	let backlink_ok = std::fs::read_to_string(&backlink)
		.ok()
		.is_some_and(|raw| canonical_eq(&resolve_pointer(admin, &raw), &dotgit));
	if !backlink_ok {
		std::fs::write(
			&backlink,
			format!("{}\n", pointer(admin, &dotgit, relative)),
		)
		.map_err(|error| anyhow!("updating {}: {error}", backlink.display()))?;
		// git reports the file's normalised absolute path (the pointer itself keeps its form).
		eprintln!(
			"repair: gitdir incorrect: {}",
			canonical(&backlink).display()
		);
	}

	// `<checkout>/.git` → the admin directory.
	let dotgit_ok = gitfile_target(&dotgit).is_some_and(|target| canonical_eq(&target, admin));
	if !dotgit_ok {
		std::fs::write(
			&dotgit,
			format!("gitdir: {}\n", pointer(checkout, admin, relative)),
		)
		.map_err(|error| anyhow!("updating {}: {error}", dotgit.display()))?;
		eprintln!(
			"repair: .git file broken: {}",
			canonical(checkout).display()
		);
	}
	Ok(())
}

/// The admin directory for the checkout at `checkout`, located by its `.git` pointer's final component
/// (the admin dir's name — stable even when a relative pointer's *depth* is stale) under the reliable
/// common dir. `None` for the main worktree (whose `.git` is a directory) or an unknown admin.
fn admin_for_checkout(common: &Path, checkout: &Path) -> Option<PathBuf> {
	let content = std::fs::read_to_string(checkout.join(".git")).ok()?;
	let raw = content.lines().next()?.strip_prefix("gitdir:")?.trim();
	let name = Path::new(raw).file_name()?;
	let admin = common.join("worktrees").join(name);
	admin.join("gitdir").is_file().then_some(admin)
}

/// The checkout directory an admin records — the parent of the `.git` file its `gitdir` pointer names
/// (resolved against the admin when relative). `None` if the `gitdir` file is unreadable.
fn checkout_for_admin(admin: &Path) -> Option<PathBuf> {
	let raw = std::fs::read_to_string(admin.join("gitdir")).ok()?;
	resolve_pointer(admin, &raw).parent().map(Path::to_path_buf)
}

// ---------------------------------------------------------------------------
// worktree pointers (absolute, or git's `worktree.useRelativePaths` relative form)
// ---------------------------------------------------------------------------

/// A pointer string from `from_dir` to `target`: relative when `prefer_relative` and a relative form
/// exists, else the absolute (real) path. Mirrors git's choice between an absolute pointer and a
/// `worktree.useRelativePaths` relative one, so a move/repair preserves whichever the worktree used.
fn pointer(from_dir: &Path, target: &Path, prefer_relative: bool) -> String {
	if prefer_relative && let Some(relative) = relativize(from_dir, target) {
		return relative;
	}
	canonical(target).display().to_string()
}

/// `target` expressed relative to the directory `from_dir` (both resolved to their real paths first), as
/// git writes a `worktree.useRelativePaths` pointer. `None` when a relative form cannot be built — the
/// two share no component at all (different roots, e.g. Windows drives), or either cannot be resolved —
/// so the caller falls back to an absolute one. Two directories under one filesystem root always share
/// that root, so on Unix a relative form (however many `..`) is always produced, matching git.
fn relativize(from_dir: &Path, target: &Path) -> Option<String> {
	let from = from_dir.canonicalize().ok()?;
	let from: Vec<_> = from.components().collect();
	let to = target.canonicalize().ok()?;
	let to: Vec<_> = to.components().collect();
	let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
	// No shared component at all means no relative form exists; the caller writes an absolute pointer.
	if shared == 0 {
		return None;
	}
	let mut parts = vec!["..".to_owned(); from.len() - shared];
	parts.extend(
		to[shared..]
			.iter()
			.map(|component| component.as_os_str().to_string_lossy().into_owned()),
	);
	(!parts.is_empty()).then(|| parts.join("/"))
}

/// Resolve a worktree pointer against `base` when it is relative, leaving an absolute one unchanged —
/// git records either form (relative when `worktree.useRelativePaths` is set).
fn resolve_pointer(base: &Path, pointer: &str) -> PathBuf {
	let pointer = Path::new(pointer.trim());
	if pointer.is_absolute() {
		pointer.to_path_buf()
	} else {
		base.join(pointer)
	}
}

/// The `gitdir:` target recorded in a checkout `.git` file, resolved to an absolute path (a relative
/// pointer resolves against the checkout directory), or `None` when the file is absent or not a gitfile
/// (e.g. the main worktree, whose `.git` is a directory).
fn gitfile_target(gitfile: &Path) -> Option<PathBuf> {
	let content = std::fs::read_to_string(gitfile).ok()?;
	let raw = content.lines().next()?.strip_prefix("gitdir:")?;
	Some(resolve_pointer(gitfile.parent()?, raw))
}

/// Whether a checkout `.git` file records its `gitdir:` pointer in relative form.
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

/// Whether the worktree (admin dir `admin`, checkout `checkout`) holds an **initialized** submodule.
/// git refuses to `move` (or `remove`) such a worktree because the submodule's `.git` link would be left
/// dangling; gitana has no submodule support, so it likewise refuses rather than corrupt a git-created
/// one. An *uninitialized* submodule (a gitlink with an empty directory) is no obstacle, matching git.
///
/// The authoritative signal is `<admin>/modules/` — where git absorbs an initialized submodule's git
/// directory — which survives even a deleted working-copy `.gitmodules`; a populated in-tree submodule
/// declared by `.gitmodules` is caught as a fallback (an older, un-absorbed layout).
fn worktree_has_submodule(admin: &Path, checkout: &Path) -> bool {
	let absorbed =
		std::fs::read_dir(admin.join("modules")).is_ok_and(|mut entries| entries.next().is_some());
	absorbed || declared_submodule_initialized(checkout)
}

/// Whether a path declared in the checkout's `.gitmodules` has a populated working tree (a `.git` entry
/// present) — the fallback signal for a submodule whose git directory is not absorbed under the admin.
fn declared_submodule_initialized(checkout: &Path) -> bool {
	let Ok(modules) = std::fs::read_to_string(checkout.join(".gitmodules")) else {
		return false;
	};
	// `.gitmodules` is git-config format; each submodule section carries a `path = <dir>` line.
	modules
		.lines()
		.filter_map(|line| line.trim().strip_prefix("path"))
		.filter_map(|rest| rest.trim_start().strip_prefix('='))
		.any(|value| checkout.join(value.trim()).join(".git").exists())
}

// ---------------------------------------------------------------------------
// worktree resolution (git's `find_worktree`)
// ---------------------------------------------------------------------------

/// A worktree resolved from a user-supplied path or name: the main worktree (no admin directory) or a
/// linked one (its admin directory under `<common>/worktrees/*`). Both carry the canonical checkout
/// path git records for the worktree.
enum WorktreeRef {
	Main { path: PathBuf },
	Linked { admin: PathBuf, path: PathBuf },
}

impl WorktreeRef {
	fn path(&self) -> &Path {
		match self {
			WorktreeRef::Main { path } | WorktreeRef::Linked { path, .. } => path,
		}
	}
}

/// Resolve a worktree by git's `find_worktree` rules: first by exact canonical path, then — failing
/// that — by a unique path *suffix* at a directory boundary (so a bare name like `feature` or a tail
/// like `sub/feature` selects `.../sub/feature`). An ambiguous or unmatched suffix resolves to `None`,
/// which callers surface as "is not a working tree" (git's behaviour).
fn find_worktree(common: &Path, cwd: &Path, arg: &Path) -> Option<WorktreeRef> {
	let worktrees = enumerate_worktrees(common);
	// By exact path: canonicalise the argument against the caller's cwd and match a recorded path.
	let wanted = canonical(&absolute(cwd, arg));
	if let Some(idx) = worktrees
		.iter()
		.position(|worktree| canonical_eq(worktree.path(), &wanted))
	{
		return worktrees.into_iter().nth(idx);
	}
	// By suffix: the argument, as given, must match the tail of exactly one worktree's path.
	let suffix = arg.to_string_lossy();
	let mut hits = worktrees
		.iter()
		.enumerate()
		.filter(|(_, worktree)| path_has_dir_suffix(worktree.path(), &suffix));
	let idx = hits.next()?.0;
	if hits.next().is_some() {
		return None; // ambiguous — git reports "is not a working tree"
	}
	worktrees.into_iter().nth(idx)
}

/// Enumerate the repository's worktrees: the main worktree first (the bare repository's directory when
/// bare, else the checkout at the common dir's parent), then each linked worktree under
/// `<common>/worktrees/*` that carries a `gitdir` pointer. Paths are canonicalised for comparison.
fn enumerate_worktrees(common: &Path) -> Vec<WorktreeRef> {
	let mut out = Vec::new();
	let main_path = if repo::is_bare(common) {
		canonical(common)
	} else {
		canonical(&repo::worktree_path_of(common))
	};
	out.push(WorktreeRef::Main { path: main_path });

	if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
		let mut admins: Vec<PathBuf> = entries
			.flatten()
			.map(|entry| entry.path())
			.filter(|admin| admin.join("gitdir").is_file())
			.collect();
		admins.sort();
		for admin in admins {
			let path = canonical(&repo::worktree_path_of(&admin));
			out.push(WorktreeRef::Linked { admin, path });
		}
	}
	out
}

/// Whether `path`'s string form ends with `suffix` at a directory boundary — git's
/// `find_worktree_by_suffix` match (the suffix must begin at the start of a path component).
fn path_has_dir_suffix(path: &Path, suffix: &str) -> bool {
	let haystack = path.to_string_lossy();
	if suffix.is_empty() || suffix.len() > haystack.len() {
		return false;
	}
	let start = haystack.len() - suffix.len();
	let at_boundary = start == 0 || haystack.as_bytes()[start - 1] == b'/';
	at_boundary && &haystack[start..] == suffix
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Resolve `path` against `cwd` if it is relative (without requiring it to exist, unlike
/// `canonicalize`).
fn absolute(cwd: &Path, path: &Path) -> PathBuf {
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		cwd.join(path)
	}
}

/// Whether `dir` exists and holds any entry.
fn dir_non_empty(dir: &Path) -> Result<bool> {
	match std::fs::read_dir(dir) {
		Ok(mut entries) => Ok(entries.next().is_some()),
		// A path that exists but is not a directory (a file) counts as occupied.
		Err(_) => Ok(true),
	}
}

/// A path's canonical form. When the path itself does not exist (e.g. a worktree whose checkout was
/// deleted), canonicalise its longest existing ancestor and re-append the missing tail, so a stale
/// worktree's path still compares equal to the canonical path recorded at creation.
fn canonical(path: &Path) -> PathBuf {
	if let Ok(resolved) = path.canonicalize() {
		return resolved;
	}
	match (path.parent(), path.file_name()) {
		(Some(parent), Some(name)) if !parent.as_os_str().is_empty() => canonical(parent).join(name),
		_ => path.to_path_buf(),
	}
}

/// Compare two paths by their canonical form.
fn canonical_eq(a: &Path, b: &Path) -> bool {
	canonical(a) == canonical(b)
}
