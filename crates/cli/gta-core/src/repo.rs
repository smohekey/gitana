//! Local repository construction over the reusable [`gitana_repository_layout`] discovery API.
//!
//! Discovery itself — walking up to a repository, resolving `.git` files, `commondir`, and bare
//! repositories to a canonical [`RepositoryLayout`] — lives in `gitana-repository-layout` and is re-exported
//! here. This module keeps the gta-specific pieces: minting filesystem capabilities from the
//! discovered paths, installing git's effective config, and the worktree-checkout guards.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;

use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};

use crate::{Backend, WorkDir};

pub use gitana_repository_layout::{DiscoveryError, RepositoryLayout, discover, try_discover};

/// The error for a work-tree operation run in a bare repo (or outside a work tree).
fn work_tree_required() -> anyhow::Error {
	anyhow!("this operation must be run in a work tree")
}

/// Open the repository whose per-worktree files live under `git_dir` and whose shared files live
/// under `common_dir`, under an explicit hash algorithm `H`. (The two are the same path for an
/// ordinary, non-linked repository.) The runtime dispatch (see [`crate::dispatch`]) picks `H` from
/// the repo's config and calls this, so each command body is monomorphised once per algorithm.
///
/// This is the program edge — the one place gta mints ambient filesystem authority from a path — so
/// it also assembles git's effective (merged) configuration here ([`crate::git_config`] needs the
/// same ambient path access) and installs it on the repository. The engine then honours a
/// global/system `remote.*` / `pack.packSizeLimit` / `core.logallrefupdates`, matching git; a
/// malformed global/system file aborts the command, as git does. A repo whose local `config` does
/// not exist yet (a fresh `init`/`clone`) still resolves the global and system layers.
pub async fn open_generic<H: HashAlgorithm>(
	git_dir: &Path,
	common_dir: &Path,
) -> Result<Repository<Backend, H>> {
	// The store is capability-pure: open the (already-created) directories here, at the
	// program edge, and hand the capabilities in.
	let common = Dir::open_ambient_dir(common_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", common_dir.display()))?;
	let git = Dir::open_ambient_dir(git_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?;
	let mut repo = Repository::new(ObjectStore::new(WorktreeFileStore::new(common, git)));
	// The effective config includes this worktree's `config.worktree` layer when
	// `extensions.worktreeConfig` is set, matching git's precedence (system < global < local <
	// config.worktree). `for_worktree` degrades to `from_repo` when the extension is off or the file is
	// absent, so an ordinary repository is unaffected.
	repo.set_effective_config(crate::git_config::for_worktree(common_dir, git_dir).await?);
	Ok(repo)
}

/// Open the working tree at `work` as a filesystem capability. Like [`open_generic`], this is a
/// program-edge point that mints ambient authority from a path — the working-tree counterpart to the
/// git-directory capability the store holds.
pub fn open_work_dir(work: &Path) -> Result<WorkDir> {
	let dir = Dir::open_ambient_dir(work, ambient_authority())
		.map_err(|error| anyhow!("opening work tree {}: {error}", work.display()))?;
	Ok(CapWorkDir::from_dir(dir))
}

/// Discover the working tree containing `start` as a [`RepositoryLayout`] plus the pathspec `prefix`,
/// without constructing a typed `WorkTree`. The runtime dispatch needs the paths so it can build a
/// `WorkTree<_, H>` for whichever hash algorithm the repo uses. The prefix is the `/`-joined path
/// from the work-tree root down to `start` (empty at the root), making pathspecs relative to the
/// caller's subdirectory, the way `git -C <subdir>` does.
pub async fn discover_worktree_with_prefix(start: &Path) -> Result<(RepositoryLayout, String)> {
	// Resolve symlinks before discovering, so the prefix reflects the physical location of the
	// caller's directory under the work tree (e.g. `-C linksub` where `linksub -> sub`).
	// Otherwise the lexical name would be matched/recorded as a tracked path. Discovery canonicalizes
	// internally too, so `worktree_root` is a canonical ancestor of this canonical `start` — the strip
	// below stays purely lexical.
	let start = std::fs::canonicalize(start)?;
	let found = discover(&start).await?;
	let work = found
		.worktree_root
		.as_ref()
		.ok_or_else(work_tree_required)?;
	// `work` is the canonical `start` with trailing components removed, so this strip succeeds.
	let prefix = start
		.strip_prefix(work)
		.unwrap_or(Path::new(""))
		.components()
		.map(|component| component.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/");
	Ok((found, prefix))
}

/// If `branch` (a full ref like `refs/heads/main`) is checked out in a *different* worktree of this
/// repository, return that worktree's working directory. git forbids checking out one branch in two
/// worktrees at once: the branch ref is shared, so the two checkouts would race when committing.
///
/// `git_dir` is the current checkout's per-worktree git directory, excluded from the scan (switching
/// to the branch this worktree is already on is not a conflict). Errors if `git_dir`'s `commondir` is
/// corrupt (rather than silently treating the repository as self-contained).
pub async fn branch_checked_out_elsewhere(git_dir: &Path, branch: &str) -> Result<Option<PathBuf>> {
	let common_dir = gitana_repository_layout::common_dir_of(git_dir).await?;
	Ok(branch_checkout_location(&common_dir, branch, Some(git_dir)))
}

/// The working directory of a worktree (the main one, or a linked one under `common_dir`) whose
/// `HEAD` is the symbolic ref `branch`, skipping `exclude` (a git directory to ignore — typically the
/// caller's own). git shares a branch ref across a repository's worktrees, so a branch may be checked
/// out in at most one at a time; this locates that one.
///
/// Unlike [`branch_checked_out_elsewhere`], `common_dir` is passed directly, so this works before the
/// caller's own git directory exists (as `worktree add` needs, checking a not-yet-created worktree).
pub(crate) fn branch_checkout_location(
	common_dir: &Path,
	branch: &str,
	exclude: Option<&Path>,
) -> Option<PathBuf> {
	let exclude = exclude.map(canonical);
	worktree_git_dirs(common_dir)
		.into_iter()
		.find(|candidate| {
			exclude.as_ref() != Some(&canonical(candidate))
				&& head_symbolic_target(candidate).as_deref() == Some(branch)
		})
		.map(|candidate| worktree_path_of(&candidate))
}

/// Every branch checked out in a worktree of this repository — the main one and each linked one —
/// paired with that worktree's working directory. This is the set a plain `fetch` (or `pull`) must
/// refuse to update: git shares a branch ref across a repository's worktrees, so fetching directly into
/// a branch a worktree has checked out would desync that checkout's index/work tree from its ref.
///
/// The *current* worktree is included; the fetch guard tells it apart from the others by `HEAD` (a
/// `pull` may still advance the current branch via its merge step, whereas any other checked-out branch
/// is refused outright). Detached / unborn worktrees contribute nothing.
pub(crate) fn branch_checkouts(common_dir: &Path) -> Vec<(String, PathBuf)> {
	worktree_git_dirs(common_dir)
		.into_iter()
		.filter_map(|candidate| {
			head_symbolic_target(&candidate).map(|branch| (branch, worktree_path_of(&candidate)))
		})
		.collect()
}

/// Every worktree's git directory for the repository at `common_dir`: the main worktree (`common_dir`
/// itself, unless the repo is bare, where its HEAD is not a checkout) and each
/// `<common_dir>/worktrees/<name>`.
fn worktree_git_dirs(common_dir: &Path) -> Vec<PathBuf> {
	let mut git_dirs = Vec::new();
	if !is_bare(common_dir) {
		git_dirs.push(common_dir.to_path_buf());
	}
	if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
		for entry in entries.flatten() {
			if entry.path().join("HEAD").is_file() {
				git_dirs.push(entry.path());
			}
		}
	}
	git_dirs
}

/// The symbolic ref `<git_dir>/HEAD` points at (e.g. `refs/heads/main`), or `None` when HEAD is
/// detached (a raw object id) or unreadable.
fn head_symbolic_target(git_dir: &Path) -> Option<String> {
	std::fs::read_to_string(git_dir.join("HEAD"))
		.ok()
		.and_then(|head| {
			head
				.strip_prefix("ref:")
				.map(|target| target.trim().to_owned())
		})
}

/// The working directory for a worktree named by its git directory, matching git's own resolution:
///
/// - A **linked** worktree's admin directory carries a `gitdir` backlink to the worktree's `.git`
///   file; its parent is the working directory (git's `get_linked_worktree`).
/// - The **main** worktree is the common directory with a trailing `/.git` stripped (git's
///   `get_main_worktree`): an ordinary `<work>/.git` yields `<work>`, while a git directory detached
///   from its work tree — `--separate-git-dir` or a symlinked `.git`, canonicalized by discovery —
///   yields the git directory itself. git resolves this from the common directory alone, ignoring the
///   real working tree and `core.worktree`, so the result is the same from any worktree.
pub(crate) fn worktree_path_of(git_dir: &Path) -> PathBuf {
	if let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) {
		// `gitdir` points at the worktree's `.git` file; its parent is the working directory. git may
		// write a relative pointer (`worktree.useRelativePaths` / `--relative-paths`), resolved against
		// the admin directory — not the caller's cwd.
		let pointer = Path::new(text.trim());
		let git_file = if pointer.is_absolute() {
			pointer.to_path_buf()
		} else {
			git_dir.join(pointer)
		};
		if let Some(parent) = git_file.parent() {
			return parent.to_path_buf();
		}
	}
	// The main worktree: strip a trailing `.git` component from the common (git) directory.
	if git_dir.file_name() == Some(OsStr::new(".git"))
		&& let Some(parent) = git_dir.parent()
	{
		return parent.to_path_buf();
	}
	git_dir.to_path_buf()
}

/// Whether the repository at `common_dir` is bare (`core.bare`), so it has no main working tree and
/// its `HEAD` is not a checkout. Uses git's boolean grammar (`true`/`yes`/`on`/`1`/valueless), not a
/// literal `"true"` match.
pub(crate) fn is_bare(common_dir: &Path) -> bool {
	std::fs::read_to_string(common_dir.join("config"))
		.ok()
		.and_then(|text| gitana_config::GitConfig::parse(&text).ok())
		.and_then(|config| config.get_bool("core", None, "bare").ok().flatten())
		.unwrap_or(false)
}

fn canonical(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
