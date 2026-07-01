//! Local repository discovery and construction.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;

use gitana_file_store_local::WorktreeFileStore;

use crate::Backend;

/// A discovered repository: where its working tree, its (possibly per-worktree) git directory, and
/// its shared *common* directory live.
pub struct Discovered {
	/// The working-tree root, or `None` for a bare repository.
	pub work: Option<PathBuf>,
	/// The git directory holding this checkout's per-worktree files (`HEAD`, `index`, ...). For an
	/// ordinary repository this is `<work>/.git`; for a linked worktree it is
	/// `<main>/.git/worktrees/<name>`.
	pub git_dir: PathBuf,
	/// The shared directory holding `objects`, `refs`, `config`, ... For an ordinary repository this
	/// equals `git_dir`; for a linked worktree it is the main `.git`.
	pub common_dir: PathBuf,
}

/// Walk up from `start` to find a repository: the nearest ancestor with a `.git` entry (its work
/// tree), or a directory that is itself a git directory (a bare repo).
///
/// `.git` may be a directory (an ordinary repository) or a file pointing at a per-worktree git
/// directory (a linked worktree created by `git worktree add`); the latter names its shared common
/// directory via a `commondir` file.
pub fn discover(start: &Path) -> Result<Discovered> {
	let mut dir = start.to_path_buf();
	loop {
		let git = dir.join(".git");
		if git.is_dir() {
			return Ok(Discovered {
				work: Some(dir),
				common_dir: git.clone(),
				git_dir: git,
			});
		}
		if git.is_file() {
			let (git_dir, common_dir) = resolve_gitdir_file(&git)?;
			return Ok(Discovered {
				work: Some(dir),
				git_dir,
				common_dir,
			});
		}
		if is_git_dir(&dir) {
			return Ok(Discovered {
				work: None,
				common_dir: dir.clone(),
				git_dir: dir,
			});
		}
		if !dir.pop() {
			bail!(
				"not a gitana repository (or any parent up to /): {}",
				start.display()
			);
		}
	}
}

/// Resolve a linked worktree's `.git` file to `(git_dir, common_dir)`. The file holds a single
/// `gitdir: <path>` line naming the per-worktree git directory; that directory holds a `commondir`
/// file naming the shared common directory (usually relative, e.g. `../..`).
fn resolve_gitdir_file(git_file: &Path) -> Result<(PathBuf, PathBuf)> {
	let content = std::fs::read_to_string(git_file)
		.map_err(|error| anyhow!("reading {}: {error}", git_file.display()))?;
	let pointer = content
		.lines()
		.next()
		.and_then(|line| line.strip_prefix("gitdir:"))
		.map(str::trim)
		.filter(|path| !path.is_empty())
		.ok_or_else(|| anyhow!("malformed .git file: {}", git_file.display()))?;
	// git writes an absolute path here; resolve a relative one against the worktree directory.
	let git_dir = match Path::new(pointer) {
		path if path.is_absolute() => path.to_path_buf(),
		path => git_file.parent().unwrap_or(Path::new(".")).join(path),
	};

	let common_dir = common_dir_of(&git_dir);
	Ok((git_dir, common_dir))
}

/// The shared common directory for a (possibly per-worktree) `git_dir`: resolved from its
/// `commondir` file, or `git_dir` itself when there is none (an ordinary or main git directory).
fn common_dir_of(git_dir: &Path) -> PathBuf {
	match std::fs::read_to_string(git_dir.join("commondir")) {
		Ok(text) => {
			let common = git_dir.join(text.trim());
			// `commondir` is typically `../..`; canonicalise so later path comparisons are clean.
			common.canonicalize().unwrap_or(common)
		}
		// No `commondir`: a self-contained git directory shares nothing.
		Err(_) => git_dir.to_path_buf(),
	}
}

/// Whether `dir` is itself a git directory (as a bare repo is): it holds `HEAD`, `objects/`, and
/// `refs/`.
fn is_git_dir(dir: &Path) -> bool {
	dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// The error for a work-tree operation run in a bare repo (or outside a work tree).
fn work_tree_required() -> anyhow::Error {
	anyhow!("this operation must be run in a work tree")
}

/// Open the repository whose per-worktree files live under `git_dir` and whose shared files live
/// under `common_dir`, under an explicit hash algorithm `H`. (The two are the same path for an
/// ordinary, non-linked repository.) The runtime dispatch (see [`crate::dispatch`]) picks `H` from
/// the repo's config and calls this, so each command body is monomorphised once per algorithm.
pub fn open_generic<H: HashAlgorithm>(
	git_dir: &Path,
	common_dir: &Path,
) -> Result<Repository<Backend, H>> {
	// The store is capability-pure: open the (already-created) directories here, at the
	// program edge, and hand the capabilities in. This is the one place gta mints ambient
	// filesystem authority from a path.
	let common = Dir::open_ambient_dir(common_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", common_dir.display()))?;
	let git = Dir::open_ambient_dir(git_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?;
	Ok(Repository::new(ObjectStore::new(WorktreeFileStore::new(
		common, git,
	))))
}

/// Discover the working tree containing `start` as a [`Discovered`] plus the pathspec `prefix`,
/// without constructing a typed `WorkTree`. The runtime dispatch needs the paths so it can build a
/// `WorkTree<_, H>` for whichever hash algorithm the repo uses. The prefix is the `/`-joined path
/// from the work-tree root down to `start` (empty at the root), making pathspecs relative to the
/// caller's subdirectory, the way `git -C <subdir>` does.
pub fn discover_worktree_with_prefix(start: &Path) -> Result<(Discovered, String)> {
	// Resolve symlinks before discovering, so the prefix reflects the physical location of the
	// caller's directory under the work tree (e.g. `-C linksub` where `linksub -> sub`).
	// Otherwise the lexical name would be matched/recorded as a tracked path.
	let start = std::fs::canonicalize(start)?;
	let found = discover(&start)?;
	let work = found.work.as_ref().ok_or_else(work_tree_required)?;
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
/// to the branch this worktree is already on is not a conflict).
pub fn branch_checked_out_elsewhere(git_dir: &Path, branch: &str) -> Option<PathBuf> {
	let common = common_dir_of(git_dir);
	let here = canonical(git_dir);

	// Every worktree's git directory: the main worktree (`common`, unless the repo is bare, where its
	// HEAD is not a checkout) and each `<common>/worktrees/<name>`.
	let mut git_dirs = Vec::new();
	if !is_bare(&common) {
		git_dirs.push(common.clone());
	}
	if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
		for entry in entries.flatten() {
			if entry.path().join("HEAD").is_file() {
				git_dirs.push(entry.path());
			}
		}
	}

	git_dirs
		.into_iter()
		.find(|candidate| canonical(candidate) != here && head_points_at(candidate, branch))
		.map(|candidate| worktree_path_of(&candidate))
}

/// Whether `<git_dir>/HEAD` is the symbolic ref `branch` (e.g. `refs/heads/main`).
fn head_points_at(git_dir: &Path, branch: &str) -> bool {
	std::fs::read_to_string(git_dir.join("HEAD"))
		.ok()
		.and_then(|head| {
			head
				.strip_prefix("ref:")
				.map(|target| target.trim().to_owned())
		})
		.is_some_and(|target| target == branch)
}

/// The working directory for a worktree named by its git directory, for error messages: the parent
/// of the `.git` file a linked worktree's `gitdir` points at, or the parent of the main `.git`.
fn worktree_path_of(git_dir: &Path) -> PathBuf {
	if let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) {
		// `gitdir` points at the worktree's `.git` file; its parent is the working directory.
		if let Some(parent) = Path::new(text.trim()).parent() {
			return parent.to_path_buf();
		}
	}
	// The main worktree: `git_dir` is `<work>/.git`.
	git_dir.parent().unwrap_or(git_dir).to_path_buf()
}

/// Whether the repository at `common_dir` is bare (`core.bare`), so it has no main working tree and
/// its `HEAD` is not a checkout. Uses git's boolean grammar (`true`/`yes`/`on`/`1`/valueless), not a
/// literal `"true"` match.
fn is_bare(common_dir: &Path) -> bool {
	std::fs::read_to_string(common_dir.join("config"))
		.ok()
		.and_then(|text| gitana_config::GitConfig::parse(&text).ok())
		.and_then(|config| config.get_bool("core", None, "bare").ok().flatten())
		.unwrap_or(false)
}

fn canonical(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
