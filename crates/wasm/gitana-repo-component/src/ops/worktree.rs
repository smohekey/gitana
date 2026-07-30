//! Working-tree operations over a [`WorkTree`]: status, add, checkout, commit.
//!
//! These require a repository opened with a work-dir descriptor (`open-worktree`); the guest holds
//! the working tree as `WorkTree<WorktreeFileStore, DescriptorWorkDir, H>` (see
//! [`crate::inner::Held`]).

use gitana_file_store_local::{DescriptorWorkDir, WorktreeFileStore};
use gitana_object::HashAlgorithm;
use gitana_worktree::{SparseReapply, SparseSet, WorkTree};

use crate::bindings::exports::gitana::repo::porcelain::{
	RepoError, SparseOutcome as WitSparseOutcome, SparsePatterns as WitSparsePatterns,
	StatusEntry as WitStatusEntry, WorktreeStatus as WitWorktreeStatus,
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
	force: bool,
) -> Result<(), RepoError> {
	let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
	wt.add(&specs, prefix, force).await.map_err(worktree_error)
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

pub(crate) async fn sparse_set<H: HashAlgorithm>(
	wt: &Tree<H>,
	patterns: Vec<String>,
	cone: bool,
) -> Result<WitSparseOutcome, RepoError> {
	let set = if cone {
		let dirs = cone_dirs(patterns)?;
		reject_tracked_files(wt, &dirs).await?;
		SparseSet::Cone(dirs)
	} else if patterns.is_empty() {
		// An empty non-cone set is git's non-cone default — root files only (`/*` then `!/*/`) — not an
		// empty pattern file, which would omit even the root files. Matches the native CLI and the WIT
		// contract that an empty set initializes to root files.
		SparseSet::NonCone(vec!["/*".to_owned(), "!/*/".to_owned()])
	} else {
		SparseSet::NonCone(patterns)
	};
	let outcome = wt.apply_sparse_set(&set).await.map_err(worktree_error)?;
	Ok(sparse_outcome(outcome))
}

/// Reject a cone directory argument that exactly names a tracked stage-0 *file*: a cone set takes
/// directories, and a file argument would render a directory pattern that can never include it. git and
/// the native CLI both refuse it ("a tracked file, not a directory"); the component applies the same
/// index check so it does not silently persist a broken set.
async fn reject_tracked_files<H: HashAlgorithm>(
	wt: &Tree<H>,
	dirs: &[String],
) -> Result<(), RepoError> {
	let index = wt.load_index().await.map_err(worktree_error)?;
	for dir in dirs {
		if !dir.is_empty()
			&& index
				.entries
				.iter()
				.any(|entry| entry.stage == 0 && entry.path == *dir)
		{
			return Err(RepoError::Invalid(format!(
				"'{dir}' is a tracked file, not a directory"
			)));
		}
	}
	Ok(())
}

/// Validate and normalise cone directory arguments the way git (and the CLI) require: a cone directory
/// is a literal path, so reject glob metacharacters and backslashes and a leading slash, collapse
/// `.`/`..`, and strip surrounding slashes. Unlike the CLI there is no invocation prefix here (the
/// component operates on an opened work tree), so directories are root-relative. Without this a raw
/// pattern such as `*` reaches `SparseSet::Cone` and renders an invalid cone file (`/*/`) that silently
/// falls back to non-cone matching — broadening the checkout and reporting `cone=false`.
fn cone_dirs(patterns: Vec<String>) -> Result<Vec<String>, RepoError> {
	patterns
		.into_iter()
		.map(|dir| {
			if dir.contains(['*', '?', '[', ']', '\\']) {
				return Err(RepoError::Invalid(format!(
					"'{dir}' contains a pattern character; cone directories must be literal paths"
				)));
			}
			if dir.starts_with('/') {
				return Err(RepoError::Invalid(format!(
					"'{dir}': cone directories must not start with a slash"
				)));
			}
			let mut components: Vec<&str> = Vec::new();
			for segment in dir.split('/') {
				match segment {
					"" | "." => {}
					".." => {
						if components.pop().is_none() {
							return Err(RepoError::Invalid(format!(
								"'{dir}' is outside the repository"
							)));
						}
					}
					segment => components.push(segment),
				}
			}
			Ok(components.join("/"))
		})
		.collect()
}

pub(crate) async fn sparse_add<H: HashAlgorithm>(
	wt: &Tree<H>,
	patterns: Vec<String>,
) -> Result<WitSparseOutcome, RepoError> {
	let current = wt.current_sparse_set().await.map_err(worktree_error)?;
	// `add` keeps the configured mode, extending the current set; sparse must already be enabled.
	let merged = match current {
		Some(SparseSet::Cone(mut dirs)) => {
			let added = cone_dirs(patterns)?;
			reject_tracked_files(wt, &added).await?;
			dirs.extend(added);
			SparseSet::Cone(dirs)
		}
		Some(SparseSet::NonCone(mut lines)) => {
			lines.extend(patterns);
			SparseSet::NonCone(lines)
		}
		None => {
			return Err(RepoError::Invalid(
				"sparse-checkout is not enabled; call sparse-set first".to_owned(),
			));
		}
	};
	let outcome = wt.apply_sparse_set(&merged).await.map_err(worktree_error)?;
	Ok(sparse_outcome(outcome))
}

pub(crate) async fn sparse_list<H: HashAlgorithm>(
	wt: &Tree<H>,
) -> Result<Option<WitSparsePatterns>, RepoError> {
	let set = wt.current_sparse_set().await.map_err(worktree_error)?;
	Ok(set.map(|set| WitSparsePatterns {
		cone: set.is_cone(),
		entries: set.entries().to_vec(),
	}))
}

pub(crate) async fn sparse_disable<H: HashAlgorithm>(
	wt: &Tree<H>,
) -> Result<WitSparseOutcome, RepoError> {
	let outcome = wt.disable_sparse().await.map_err(worktree_error)?;
	Ok(sparse_outcome(outcome))
}

pub(crate) async fn sparse_reapply<H: HashAlgorithm>(
	wt: &Tree<H>,
) -> Result<WitSparseOutcome, RepoError> {
	let outcome = wt.reapply_sparse().await.map_err(worktree_error)?;
	Ok(sparse_outcome(outcome))
}

/// Map the engine reapply outcome to the boundary record.
fn sparse_outcome(outcome: SparseReapply) -> WitSparseOutcome {
	WitSparseOutcome {
		left_dirty: outcome.left_dirty,
		not_updated: outcome.not_updated,
	}
}

/// Map a [`gitana_porcelain::CommitError`] to the boundary error. The three refusals are invalid
/// input; the underlying index/repository failures defer to the shared mappings so precise variants
/// (`corruption`, `ref-moved`, …) survive the boundary rather than collapsing to `backend`.
fn commit_error(error: gitana_porcelain::CommitError) -> RepoError {
	use gitana_porcelain::CommitError;
	match error {
		CommitError::Index(error) => worktree_error(error),
		CommitError::Repository(error) => repo_error(error),
		// The component's `commit` op never signs, so `Signing` cannot arise here; map it like
		// `Identity` (an opaque signer failure) to keep the match total.
		CommitError::Identity(error) | CommitError::Signing(error) => {
			RepoError::Invalid(format!("{error:#}"))
		}
		refusal @ (CommitError::Unmerged | CommitError::Empty | CommitError::NothingToCommit) => {
			RepoError::Invalid(refusal.to_string())
		}
	}
}
