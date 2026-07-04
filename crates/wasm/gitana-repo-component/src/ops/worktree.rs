//! Working-tree operations over a [`WorkTree`]: status, add, checkout, commit.
//!
//! These require a repository opened with a work-dir descriptor (`open-worktree`); the guest holds
//! the working tree as `WorkTree<WorktreeFileStore, DescriptorWorkDir, H>` (see
//! [`crate::inner::Held`]).

use gitana_file_store_local::{DescriptorWorkDir, WorktreeFileStore};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::bindings::exports::gitana::repo::porcelain::{
	RepoError, StatusEntry as WitStatusEntry, WorktreeStatus as WitWorktreeStatus,
};

use super::{repo_error, worktree_error};

/// The working tree the worktree ops run over — the concrete `W` is the wasm descriptor capability.
type Tree<H> = WorkTree<WorktreeFileStore, DescriptorWorkDir, H>;

pub(crate) async fn status<H: HashAlgorithm>(wt: &Tree<H>) -> Result<WitWorktreeStatus, RepoError> {
	let status = wt.status().await.map_err(worktree_error)?;
	Ok(WitWorktreeStatus {
		changed: status
			.changed
			.into_iter()
			.map(|entry| WitStatusEntry {
				path: entry.path,
				index: entry.index.to_string(),
				worktree: entry.worktree.to_string(),
			})
			.collect(),
		untracked: status.untracked,
	})
}

pub(crate) async fn add<H: HashAlgorithm>(
	wt: &Tree<H>,
	pathspecs: &[String],
	prefix: &str,
) -> Result<(), RepoError> {
	let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
	wt.add(&specs, prefix).await.map_err(worktree_error)
}

pub(crate) async fn checkout<H: HashAlgorithm>(
	wt: &Tree<H>,
	tree_ish: &str,
	force: bool,
) -> Result<(), RepoError> {
	// Resolve the spec (commit/tag/tree) and peel it to the tree checkout materialises.
	let id = wt.rev_parse(tree_ish).await.map_err(worktree_error)?;
	let tree = wt.repository().peel_to_tree(id).await.map_err(repo_error)?;
	wt.checkout(tree, force).await.map_err(worktree_error)
}

pub(crate) async fn commit<H: HashAlgorithm>(
	wt: &Tree<H>,
	message: &str,
	author: &str,
	committer: &str,
) -> Result<String, RepoError> {
	// The `gitana_porcelain::commit` orchestration, reimplemented here so the component need not
	// depend on gitana-porcelain (which would pull gitana-remote → reqwest into the wasip2 reactor).
	// Identity is passed in, not resolved from env/config, since the component has neither.
	let index = wt.load_index().await.map_err(worktree_error)?;
	// An unmerged index would silently drop conflicted paths (no stage-0 entry) from the tree.
	if index.has_conflicts() {
		return Err(RepoError::Invalid(
			"committing is not possible because you have unmerged files; resolve them first".to_owned(),
		));
	}
	let entries = index.tree_entries();
	if entries.is_empty() {
		return Err(RepoError::Invalid(
			"nothing to commit (empty index)".to_owned(),
		));
	}
	let repo = wt.repository();
	let tree = repo.write_tree(&entries).await.map_err(repo_error)?;
	// Refuse a commit that would not change the tree — git's "nothing to commit, working tree clean"
	// (the initial commit, with no parent tree to match, is always allowed).
	if let Some(head) = repo.refs().resolve_head().await.map_err(repo_error)?
		&& repo.commit_tree(head).await.map_err(repo_error)? == tree
	{
		return Err(RepoError::Invalid(
			"nothing to commit, working tree clean".to_owned(),
		));
	}
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	let id = repo
		.commit_on_head(tree, author, committer, &message)
		.await
		.map_err(repo_error)?;
	Ok(id.to_hex())
}
