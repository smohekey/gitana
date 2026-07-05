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

use super::{HostIdentity, repo_error, worktree_error};

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
	// Reuse `gitana_porcelain::commit` (the unmerged/empty/unchanged guards, tree write, and commit),
	// supplying identity as the host-passed lines. `CommitError` keeps the refusals distinct from a
	// store failure, so they map to `invalid` vs `backend` at the boundary.
	let identity = HostIdentity { author, committer };
	let id = gitana_porcelain::commit(wt, message, &identity)
		.await
		.map_err(commit_error)?;
	Ok(id.to_hex())
}

/// Map a [`gitana_porcelain::CommitError`] to the boundary error. The three refusals are invalid
/// input; the underlying index/repository failures defer to the shared mappings so precise variants
/// (`corruption`, `ref-moved`, …) survive the boundary rather than collapsing to `backend`.
fn commit_error(error: gitana_porcelain::CommitError) -> RepoError {
	use gitana_porcelain::CommitError;
	match error {
		CommitError::Index(error) => worktree_error(error),
		CommitError::Repository(error) => repo_error(error),
		CommitError::Identity(error) => RepoError::Invalid(format!("{error:#}")),
		refusal @ (CommitError::Unmerged | CommitError::Empty | CommitError::NothingToCommit) => {
			RepoError::Invalid(refusal.to_string())
		}
	}
}
