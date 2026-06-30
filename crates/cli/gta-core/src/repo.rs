//! Local repository discovery and construction.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;

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

/// Open the repository rooted at `git_dir` under an explicit hash algorithm `H`. The
/// runtime dispatch (see [`crate::dispatch`]) picks `H` from the repo's config and calls
/// this, so each command body is monomorphised once per algorithm.
pub fn open_generic<H: HashAlgorithm>(git_dir: &Path) -> Repository<LocalFileStore, H> {
	Repository::new(ObjectStore::new(LocalFileStore::new(git_dir)))
}

/// Discover the working tree containing `start` as `(work_dir, git_dir, prefix)`, without
/// constructing a typed `WorkTree`. The runtime dispatch needs the paths so it can build
/// a `WorkTree<_, H>` for whichever hash algorithm the repo uses. The prefix is the
/// `/`-joined path from the work-tree root down to `start` (empty at the root), making
/// pathspecs relative to the caller's subdirectory, the way `git -C <subdir>` does.
pub fn discover_worktree_with_prefix(start: &Path) -> Result<(PathBuf, PathBuf, String)> {
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
	Ok((work, git, prefix))
}
