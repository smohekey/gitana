//! `gta worktree` — create, list, and remove linked working trees (`git worktree add/list/remove`).
//!
//! gta already *operates inside* a linked worktree (see [`crate::repo`]); this command *creates* the
//! layout git reads. A linked worktree is an admin directory `<common>/worktrees/<name>/` holding the
//! per-worktree files (`HEAD`, `index`, `commondir` → the shared `.git`, `gitdir` → the checkout's
//! `.git` file) plus a checkout whose `.git` is a file pointing back at that admin directory.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::dispatch::detect_algorithm;
use crate::repo::{self, Discovered};
use crate::{Backend, WorkDir};

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
	let found = repo::discover(cwd)?;
	let target = absolute(cwd, path);
	// git refuses an existing, non-empty destination (an empty directory is fine).
	if target.exists() && dir_non_empty(&target)? {
		bail!("'{}' already exists", path.display());
	}
	// A path already registered under `.git/worktrees` — even one whose checkout was deleted — must not
	// be re-added, or the repository ends up with two admin entries for one path. git refuses this;
	// clear it first with `worktree remove`.
	if admin_dir_for(&found.common_dir, &canonical(&target)).is_some() {
		bail!(
			"'{}' is a missing but already registered worktree",
			path.display()
		);
	}
	match detect_algorithm(&found.common_dir)? {
		HashKind::Sha1 => {
			add_generic::<Sha1>(&found, &target, commit_ish, branch, force_branch, detach).await
		}
		HashKind::Sha256 => {
			add_generic::<Sha256>(&found, &target, commit_ish, branch, force_branch, detach).await
		}
	}
}

async fn add_generic<H: HashAlgorithm>(
	found: &Discovered,
	target: &Path,
	commit_ish: Option<&str>,
	branch: Option<&str>,
	force_branch: bool,
	detach: bool,
) -> Result<()> {
	let common = &found.common_dir;
	// Resolve the start point and the checkout mode against the *current* repository (its refs and
	// objects are shared with the new worktree, so either repo resolves them identically).
	let repo = repo::open_generic::<H>(&found.git_dir, common)?;
	let plan = plan_checkout::<H>(&repo, target, commit_ish, branch, force_branch, detach).await?;

	// The admin directory name is the destination's basename, uniquified against existing worktrees
	// (git appends a numeric suffix on collision).
	let base = target
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| anyhow!("invalid worktree path: '{}'", target.display()))?;
	let admin = unique_admin_dir(common, base);

	// A *born* branch's ref is shared across the repository's worktrees, so git forbids checking one
	// out in two at once. Refuse before writing anything. (The new worktree does not exist yet, so no
	// exclusion is needed — every existing worktree is a real conflict, including the current one.) An
	// unborn branch (an orphan plan, `commit` = None) has no ref to race on, so git allows a second
	// worktree to point `HEAD` at it — skip the guard there.
	if plan.commit.is_some()
		&& let HeadState::Symbolic(refname) = &plan.head
		&& let Some(other) = repo::branch_checkout_location(common, refname, None)
	{
		bail!(
			"'{}' is already checked out at '{}'",
			refname.strip_prefix("refs/heads/").unwrap_or(refname),
			other.display()
		);
	}

	// Create the branch first (a shared ref), then materialise the admin directory and checkout. A
	// `create` is only ever paired with a concrete commit (never an orphan).
	if let Some((refname, expected)) = &plan.create {
		let commit = plan
			.commit
			.expect("a branch to create implies a start commit");
		repo.refs().update_ref(refname, commit, *expected).await?;
	}

	let admin = write_admin_layout(&admin, target, &plan.head, plan.commit)?;

	// Open the new worktree (per-worktree files under `admin`, shared files under `common`) and
	// materialise the checkout. An orphan worktree has no commit, so it is left empty (as git does).
	if let Some(commit) = plan.commit {
		let new_repo = repo::open_generic::<H>(&admin, common)?;
		let work: WorkDir = repo::open_work_dir(target)?;
		let worktree = WorkTree::new(new_repo, work, admin.clone());
		let tree = worktree.repository().commit_tree(commit).await?;
		worktree.checkout(tree, false).await?;
	}

	report_add(&plan.label, plan.commit);
	Ok(())
}

/// The checkout the `add` will perform, decided from the DWIM rules before any state is written.
struct Plan<H: HashAlgorithm> {
	/// `HEAD` to write in the new worktree: a branch (symbolic) or a detached commit.
	head: HeadState<H>,
	/// The commit to check out (the tree source, and the recorded `ORIG_HEAD`), or `None` for an orphan
	/// worktree — a new unborn branch in a repository with no commits yet, checked out empty.
	commit: Option<ObjectId<H>>,
	/// A branch ref to create/reset before the checkout, with its expected current value for the CAS.
	/// Always `None` for an orphan (its branch is unborn, so there is no commit to point a ref at).
	create: Option<(String, Option<ObjectId<H>>)>,
	/// How to describe the checkout in the "Preparing worktree" line.
	label: Label,
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

/// Decide the checkout mode from git's DWIM rules:
/// - `--detach` → detached `HEAD` at `commit_ish` (default `HEAD`);
/// - `-b`/`-B <name>` → create (or, with `-B`, reset) branch `<name>` at `commit_ish` and check it out;
/// - a `commit_ish` that names a local branch → check that branch out; any other → detached `HEAD`;
/// - no `commit_ish` → a new branch named after the destination's basename, or, if it already exists,
///   that branch checked out.
async fn plan_checkout<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	target: &Path,
	commit_ish: Option<&str>,
	branch: Option<&str>,
	force_branch: bool,
	detach: bool,
) -> Result<Plan<H>> {
	if detach {
		// A detached HEAD needs a concrete commit: an unborn HEAD errors here, as git does.
		let commit = resolve_commit(repo, commit_ish.unwrap_or("HEAD")).await?;
		return Ok(Plan {
			head: HeadState::Detached(commit),
			commit: Some(commit),
			create: None,
			label: Label::Detached,
		});
	}

	if let Some(name) = branch {
		validate_branch_name(name)?;
		let refname = format!("refs/heads/{name}");
		let existing = repo.refs().resolve(&refname).await?;
		if existing.is_some() && !force_branch {
			bail!("a branch named '{name}' already exists");
		}
		// With no explicit start point in a repository that has no commits at all, git infers an orphan:
		// the new branch is unborn, so there is no commit to check out or point the ref at.
		if commit_ish.is_none() && is_empty_repo(repo).await? {
			return Ok(Plan {
				head: HeadState::Symbolic(refname),
				commit: None,
				create: None,
				label: Label::NewBranch(name.to_owned()),
			});
		}
		// Otherwise a start point is required: an unborn HEAD in a non-empty repo errors here, as git
		// does (it does not silently orphan onto an existing branch).
		let commit = resolve_commit(repo, commit_ish.unwrap_or("HEAD")).await?;
		return Ok(Plan {
			head: HeadState::Symbolic(refname.clone()),
			commit: Some(commit),
			create: Some((refname, existing)),
			label: Label::NewBranch(name.to_owned()),
		});
	}

	match commit_ish {
		// An explicit start point: check out a branch by that name, otherwise detach at the commit.
		Some(spec) => {
			let refname = format!("refs/heads/{spec}");
			if let Some(commit) = repo.refs().resolve(&refname).await? {
				Ok(Plan {
					head: HeadState::Symbolic(refname),
					commit: Some(commit),
					create: None,
					label: Label::CheckoutBranch(spec.to_owned()),
				})
			} else {
				let commit = resolve_commit(repo, spec).await?;
				Ok(Plan {
					head: HeadState::Detached(commit),
					commit: Some(commit),
					create: None,
					label: Label::Detached,
				})
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
			match repo.refs().resolve(&refname).await? {
				Some(commit) => Ok(Plan {
					head: HeadState::Symbolic(refname),
					commit: Some(commit),
					create: None,
					label: Label::CheckoutBranch(name.to_owned()),
				}),
				// The branch does not exist: create it at HEAD, or — in a repo with no commits at all —
				// orphan it (a new unborn branch, empty checkout).
				None if is_empty_repo(repo).await? => Ok(Plan {
					head: HeadState::Symbolic(refname),
					commit: None,
					create: None,
					label: Label::NewBranch(name.to_owned()),
				}),
				None => {
					let commit = resolve_commit(repo, "HEAD").await?;
					Ok(Plan {
						head: HeadState::Symbolic(refname.clone()),
						commit: Some(commit),
						create: Some((refname, None)),
						label: Label::NewBranch(name.to_owned()),
					})
				}
			}
		}
	}
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

/// Write the admin directory (`<common>/worktrees/<name>/`) and the checkout's `.git` file, and
/// return the canonicalised admin directory. Mirrors `git worktree add`'s on-disk layout: `HEAD`,
/// `ORIG_HEAD`, `commondir` (→ the shared `.git`), and `gitdir` (→ the checkout's `.git` file). The
/// `index` and `logs/HEAD` are created by the subsequent checkout.
fn write_admin_layout<H: HashAlgorithm>(
	admin: &Path,
	target: &Path,
	head: &HeadState<H>,
	commit: Option<ObjectId<H>>,
) -> Result<PathBuf> {
	std::fs::create_dir_all(admin)
		.map_err(|error| anyhow!("creating {}: {error}", admin.display()))?;
	std::fs::create_dir_all(target)
		.map_err(|error| anyhow!("creating {}: {error}", target.display()))?;

	// Absolute paths for the cross-pointers, so each side resolves regardless of the caller's cwd.
	let admin = admin
		.canonicalize()
		.map_err(|error| anyhow!("resolving {}: {error}", admin.display()))?;
	let target = target
		.canonicalize()
		.map_err(|error| anyhow!("resolving {}: {error}", target.display()))?;
	let git_file = target.join(".git");

	// `commondir` is relative (git writes `../..`): from `<common>/worktrees/<name>` up to `<common>`.
	std::fs::write(admin.join("commondir"), "../..\n")?;
	std::fs::write(admin.join("gitdir"), format!("{}\n", git_file.display()))?;
	std::fs::write(admin.join("HEAD"), head.render())?;
	// An orphan worktree has no start commit, so — like git — it gets no `ORIG_HEAD`.
	if let Some(commit) = commit {
		std::fs::write(admin.join("ORIG_HEAD"), format!("{commit}\n"))?;
	}
	std::fs::write(&git_file, format!("gitdir: {}\n", admin.display()))?;
	Ok(admin)
}

/// The admin directory for a new worktree named after `base`, uniquified against the existing
/// `<common>/worktrees/*` (git appends `1`, `2`, … on collision).
fn unique_admin_dir(common: &Path, base: &str) -> PathBuf {
	let worktrees = common.join("worktrees");
	if !worktrees.join(base).exists() {
		return worktrees.join(base);
	}
	for suffix in 1u32.. {
		let candidate = worktrees.join(format!("{base}{suffix}"));
		if !candidate.exists() {
			return candidate;
		}
	}
	unreachable!("a free worktree admin name always exists")
}

fn report_add<H: HashAlgorithm>(label: &Label, commit: Option<ObjectId<H>>) {
	match label {
		Label::NewBranch(name) => eprintln!("Preparing worktree (new branch '{name}')"),
		Label::CheckoutBranch(name) => eprintln!("Preparing worktree (checking out '{name}')"),
		Label::Detached => {
			let commit = commit.expect("a detached HEAD has a commit");
			eprintln!("Preparing worktree (detached HEAD {})", short(commit));
		}
	}
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(cwd: &Path, porcelain: bool) -> Result<()> {
	let found = repo::discover(cwd)?;
	let entries = match detect_algorithm(&found.common_dir)? {
		HashKind::Sha1 => collect::<Sha1>(&found.common_dir).await?,
		HashKind::Sha256 => collect::<Sha256>(&found.common_dir).await?,
	};
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

/// Gather the worktrees: the main worktree first (the bare repository itself when bare), then each
/// linked worktree under `<common>/worktrees/*`, sorted by admin-directory name (git's order).
async fn collect<H: HashAlgorithm>(common: &Path) -> Result<Vec<WorktreeInfo>> {
	let repo = repo::open_generic::<H>(common, common)?;
	let mut out = Vec::new();

	if repo::is_bare(common) {
		out.push(WorktreeInfo {
			path: canonical(common),
			state: State::Bare,
			locked: None,
			prunable: None,
		});
	} else if let Some(info) = info_for::<H>(&repo, common, &repo::worktree_path_of(common)).await? {
		out.push(info);
	}

	let mut names: Vec<String> = match std::fs::read_dir(common.join("worktrees")) {
		Ok(entries) => entries
			.flatten()
			.filter(|entry| entry.path().join("HEAD").is_file())
			.filter_map(|entry| entry.file_name().into_string().ok())
			.collect(),
		Err(_) => Vec::new(),
	};
	names.sort();
	for name in names {
		let admin = common.join("worktrees").join(&name);
		let work = repo::worktree_path_of(&admin);
		if let Some(info) = info_for::<H>(&repo, &admin, &work).await? {
			out.push(info);
		}
	}
	Ok(out)
}

/// Build a [`WorktreeInfo`] from a worktree's git directory (`git_dir`, holding its `HEAD`) and
/// working-tree path. Returns `None` only when the `HEAD` file is absent. An unborn branch (its ref
/// resolves to nothing yet) is kept, with an all-zeros `HEAD` — as git lists it.
async fn info_for<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	git_dir: &Path,
	work: &Path,
) -> Result<Option<WorktreeInfo>> {
	let Some(head) = read_head::<H>(git_dir)? else {
		return Ok(None);
	};
	let path = canonical(work);
	let state = match head {
		HeadState::Symbolic(refname) => {
			let head = match repo.refs().resolve(&refname).await? {
				Some(commit) => commit.to_hex(),
				None => zero_hex::<H>(),
			};
			State::Checkout {
				head,
				branch: Some(refname),
			}
		}
		HeadState::Detached(commit) => State::Checkout {
			head: commit.to_hex(),
			branch: None,
		},
	};
	// A worktree the user locked (a `locked` file in its git dir); a stale worktree whose checkout has
	// been deleted is prunable, as git reports.
	let locked = read_lock_reason(git_dir);
	// A stale worktree is prunable — unless it is locked, since the lock protects it (git then reports
	// only `locked`, not `prunable`).
	let prunable = (!work.exists() && locked.is_none())
		.then(|| "gitdir file points to non-existent location".to_owned());
	Ok(Some(WorktreeInfo {
		path,
		state,
		locked,
		prunable,
	}))
}

/// The lock reason for a worktree whose git directory holds a `locked` file — `Some("")` when locked
/// without a reason, `None` when unlocked. git writes the reason (if any) as the file's contents.
fn read_lock_reason(git_dir: &Path) -> Option<String> {
	match std::fs::read_to_string(git_dir.join("locked")) {
		Ok(reason) => Some(reason.trim().to_owned()),
		Err(_) => None,
	}
}

/// Parse `<git_dir>/HEAD`, or `None` if it is absent.
fn read_head<H: HashAlgorithm>(git_dir: &Path) -> Result<Option<HeadState<H>>> {
	match std::fs::read(git_dir.join("HEAD")) {
		Ok(bytes) => Ok(Some(
			HeadState::parse(&bytes).map_err(|error| anyhow!("{error}"))?,
		)),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(anyhow!("reading {}/HEAD: {error}", git_dir.display())),
	}
}

/// The all-zeros object-id hex for `H` (`2 * RAW_LEN` zeros), git's placeholder for an unborn `HEAD`.
fn zero_hex<H: HashAlgorithm>() -> String {
	"0".repeat(H::RAW_LEN * 2)
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
	let found = repo::discover(cwd)?;
	let common = &found.common_dir;
	// The checkout may already be gone (deleted or moved) — git still cleans up such a stale entry — so
	// resolve leniently rather than requiring the path to exist.
	let target = canonical(&absolute(cwd, path));

	// The main worktree cannot be removed (git: "is a main working tree").
	if !repo::is_bare(common) && canonical_eq(&repo::worktree_path_of(common), &target) {
		bail!("'{}' is a main working tree", path.display());
	}

	let admin = admin_dir_for(common, &target)
		.ok_or_else(|| anyhow!("'{}' is not a working tree", path.display()))?;

	// A locked worktree is protected: git requires two `-f` to remove it (one is not enough).
	if let Some(reason) = read_lock_reason(&admin)
		&& force < 2
	{
		if reason.is_empty() {
			bail!("cannot remove a locked working tree");
		}
		bail!("cannot remove a locked working tree, lock reason: {reason}");
	}

	// If the checkout is still present, it must genuinely belong to this admin entry before we delete
	// it — its `.git` file must point back here. git refuses (even with `--force`) when the gitfile is
	// gone or foreign, so an unrelated directory left at the recorded path is never destroyed. A wholly
	// absent checkout is a stale entry: nothing to validate, just drop the registration.
	if target.exists() {
		let gitfile = target.join(".git");
		if !gitfile.is_file() {
			bail!(
				"validation failed, cannot remove working tree: '{}' does not exist",
				gitfile.display()
			);
		}
		if !checkout_points_to(&gitfile, &admin) {
			bail!(
				"validation failed, cannot remove working tree: '{}' does not point to this worktree",
				gitfile.display()
			);
		}
	}

	// A still-present dirty checkout needs one `-f`; a stale one has nothing to check.
	if force < 1 && target.exists() {
		let dirty = match detect_algorithm(common)? {
			HashKind::Sha1 => is_dirty::<Sha1>(common, &admin, &target).await?,
			HashKind::Sha256 => is_dirty::<Sha256>(common, &admin, &target).await?,
		};
		if dirty {
			bail!(
				"'{}' contains modified or untracked files, use --force to delete it",
				path.display()
			);
		}
	}

	// Remove the checkout (if it still exists), then the admin directory, so a failure part-way leaves
	// a registered (repairable) worktree rather than an orphaned admin directory.
	if target.exists() {
		std::fs::remove_dir_all(&target)
			.map_err(|error| anyhow!("removing {}: {error}", target.display()))?;
	}
	std::fs::remove_dir_all(&admin)
		.map_err(|error| anyhow!("removing {}: {error}", admin.display()))?;
	Ok(())
}

/// Whether the worktree checked out at `target` (per-worktree files under `admin`) has staged,
/// unstaged, or untracked changes — anything git counts as "modified or untracked".
async fn is_dirty<H: HashAlgorithm>(common: &Path, admin: &Path, target: &Path) -> Result<bool> {
	let repo = repo::open_generic::<H>(admin, common)?;
	let work: WorkDir = repo::open_work_dir(target)?;
	let worktree = WorkTree::new(repo, work, admin.to_path_buf());
	let status = worktree.status().await?;
	Ok(!status.changed.is_empty() || !status.untracked.is_empty())
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

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..7.min(hex.len())].to_owned()
}
