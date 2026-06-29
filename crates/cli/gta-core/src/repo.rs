//! Local repository discovery and construction.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::ObjectId;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// A repository over the local filesystem backend.
pub type LocalRepository = Repository<LocalFileStore>;

/// A working tree over the local filesystem backend.
pub type LocalWorkTree = WorkTree<LocalFileStore>;

/// Walk up from `start` to find a repository: the nearest ancestor with a `.git` directory (its
/// work tree), or a directory that is itself a git directory (a bare repo). Returns
/// `(work_dir, git_dir)`, where `work_dir` is `None` for a bare repo.
pub fn discover(start: &Path) -> Result<(Option<PathBuf>, PathBuf)> {
	let mut dir = start.to_path_buf();
	loop {
		let git = dir.join(".git");
		if git.is_dir() {
			return Ok((Some(dir), git));
		}
		if is_git_dir(&dir) {
			return Ok((None, dir));
		}
		if !dir.pop() {
			bail!(
				"not a gitana repository (or any parent up to /): {}",
				start.display()
			);
		}
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

/// Open the repository rooted at `git_dir`.
pub fn open(git_dir: &Path) -> LocalRepository {
	Repository::new(ObjectStore::new(LocalFileStore::new(git_dir)))
}

/// Discover and open the repository containing `start`.
pub fn open_here(start: &Path) -> Result<LocalRepository> {
	let (_work, git) = discover(start)?;
	Ok(open(&git))
}

/// Resolve `spec` to an object id, returning a repository to read it with. An index-relative
/// spec (`:...`) opens the work tree, which holds the index; every other spec resolves against
/// the repository alone, so object-only lookups (`<oid>`, `<rev>:<path>`, …) do not require a
/// work tree.
pub async fn resolve_object(start: &Path, spec: &str) -> Result<(LocalRepository, ObjectId)> {
	if spec.starts_with(':') {
		let oid = open_worktree(start)?.rev_parse(spec).await?;
		Ok((open_here(start)?, oid))
	} else {
		let repo = open_here(start)?;
		let oid = repo.rev_parse(spec).await?;
		Ok((repo, oid))
	}
}

/// Discover and open the working tree containing `start`. Errors in a bare repo.
pub fn open_worktree(start: &Path) -> Result<LocalWorkTree> {
	let (work, git) = discover(start)?;
	let work = work.ok_or_else(work_tree_required)?;
	Ok(WorkTree::new(open(&git), work, git))
}

/// Discover the working tree containing `start`, plus the `/`-joined path from the work-tree
/// root down to `start` (empty at the root). The prefix makes pathspecs relative to the
/// caller's subdirectory, the way `git -C <subdir>` interprets them.
pub fn open_worktree_with_prefix(start: &Path) -> Result<(LocalWorkTree, String)> {
	// Resolve symlinks before discovering, so the prefix reflects the physical location of the
	// caller's directory under the work tree (e.g. `-C linksub` where `linksub -> sub`).
	// Otherwise the lexical name would be matched/recorded as a tracked path.
	let start = std::fs::canonicalize(start)?;
	let (work, git) = discover(&start)?;
	let work = work.ok_or_else(work_tree_required)?;
	// `work` is the canonical `start` with trailing components removed, so this strip succeeds.
	let prefix = start
		.strip_prefix(&work)
		.unwrap_or(Path::new(""))
		.components()
		.map(|component| component.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/");
	Ok((WorkTree::new(open(&git), work, git), prefix))
}
