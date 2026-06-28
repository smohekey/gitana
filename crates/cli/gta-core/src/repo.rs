//! Local repository discovery and construction.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// A repository over the local filesystem backend.
pub type LocalRepository = Repository<LocalFileStore>;

/// A working tree over the local filesystem backend.
pub type LocalWorkTree = WorkTree<LocalFileStore>;

/// Walk up from `start` to find the working tree (the nearest ancestor with a
/// `.git` directory), returning `(work_dir, git_dir)`.
pub fn discover(start: &Path) -> Result<(PathBuf, PathBuf)> {
	let mut dir = start.to_path_buf();
	loop {
		let git = dir.join(".git");
		if git.is_dir() {
			return Ok((dir, git));
		}
		if !dir.pop() {
			bail!(
				"not a gitana repository (or any parent up to /): {}",
				start.display()
			);
		}
	}
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

/// Discover and open the working tree containing `start`.
pub fn open_worktree(start: &Path) -> Result<LocalWorkTree> {
	let (work, git) = discover(start)?;
	Ok(WorkTree::new(open(&git), work, git))
}
