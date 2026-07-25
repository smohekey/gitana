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
			path,
		});
	}
	wt.save_index(&index).await
}
