//! Reset the index to a tree (the index half of `git reset --mixed`).
//!
//! Replaces every index entry with the tree's entries (stage 0), leaving the working tree
//! untouched. Entries get a default stat so `status` re-hashes the working-tree file rather than
//! trusting a stale cache. Like `checkout`/`restore`, paths from the tree are validated against
//! the checkout CVE class before they enter the index.

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::checkout::validate_path;
use crate::{Index, IndexEntry, Stat, WorkTree, WorktreeError};

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	tree: ObjectId<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let entries = wt.repository().read_tree(tree).await?;

	// Validate up front so a hostile tree path (`../x`, `.git/config`) cannot enter the index.
	for (path, _, _) in &entries {
		validate_path(path)?;
	}

	// Carry the prior entries' index-only flags forward: git's `reset` preserves `skip_worktree`
	// (a sparse path stays excluded) and `assume_valid` across the rebuild, so a reset must not
	// silently un-sparse the repository.
	let prior_flags: std::collections::HashMap<String, (bool, bool)> = wt
		.load_index()
		.await?
		.entries
		.into_iter()
		.filter(|entry| entry.stage == 0)
		.map(|entry| (entry.path, (entry.skip_worktree, entry.assume_valid)))
		.collect();
	// A path the reset introduces (no prior entry) derives its skip-worktree bit from the active sparse
	// matcher: git's unpack-trees marks an out-of-cone path skip-worktree (probed — `reset --mixed` to a
	// tree adding an excluded path leaves it `S` and absent), rather than reporting its absence a deletion.
	let sparse = wt.sparse_checkout().await?;

	let mut index = Index::new();
	for (path, mode, oid) in entries {
		let (skip_worktree, assume_valid) = match prior_flags.get(&path) {
			Some(&flags) => flags,
			None => (
				sparse
					.as_ref()
					.is_some_and(|matcher| !matcher.includes(&path)),
				false,
			),
		};
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: u32::from_str_radix(&mode, 8).unwrap_or(0o100644),
			oid,
			stage: 0,
			assume_valid,
			skip_worktree,
			intent_to_add: false,
			path,
		});
	}
	wt.save_index(&index).await
}

/// Rebuild the index from `tree` **only if `.git/index` is missing**, atomically under the index lock.
///
/// A porcelain operation whose model assumes `index == HEAD` (a merge fast-forward) uses this to repair a
/// deleted/corrupt index before delegating to the two-tree merge. The existence check and the rebuild share
/// one `index.lock` window, so a concurrent `add` cannot recreate-and-stage the index between them and then
/// have its staged work discarded: if the index already exists (created by us on a prior call, or by a
/// racing writer), this is a no-op that preserves it; only a genuinely absent index is rebuilt. Entries get
/// a default stat and derive `skip_worktree` from the active sparse matcher, as [`run`] does for new paths.
pub(crate) async fn ensure_from_tree_if_missing<F, W, H>(
	wt: &WorkTree<F, W, H>,
	tree: ObjectId<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let lock = wt.lock_index().await?;
	// Under the lock: a present index (ours from a prior call, or a racing writer's) must be left untouched.
	if wt.index_exists().await? {
		wt.release_index_lock(lock).await;
		return Ok(());
	}
	let entries = wt.repository().read_tree(tree).await?;
	for (path, _, _) in &entries {
		validate_path(path)?;
	}
	let sparse = wt.sparse_checkout().await?;
	let mut index = Index::new();
	for (path, mode, oid) in entries {
		// Prove the blob exists before recording an OID the merge will trust for `is_clean` — and thus treat
		// the matching working-tree file as disposable. A missing/corrupt blob on a rebuilt-from-HEAD index
		// would otherwise let the fast-forward delete the sole copy of a HEAD-tracked file it means to remove;
		// abort instead, as the retired path refused a missing-index state. (A gitlink is a submodule commit,
		// not a blob here.)
		if mode != "160000" {
			wt.repository().read_blob(oid).await?;
		}
		let skip_worktree = sparse
			.as_ref()
			.is_some_and(|matcher| !matcher.includes(&path));
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: u32::from_str_radix(&mode, 8).unwrap_or(0o100644),
			oid,
			stage: 0,
			assume_valid: false,
			skip_worktree,
			intent_to_add: false,
			path,
		});
	}
	// No working-tree mutation occurred, so the lock commits (writes the index + releases) cleanly.
	wt.commit_index(lock, &index).await
}
