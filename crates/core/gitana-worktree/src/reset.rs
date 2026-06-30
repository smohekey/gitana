//! Reset the index to a tree (the index half of `git reset --mixed`).
//!
//! Replaces every index entry with the tree's entries (stage 0), leaving the working tree
//! untouched. Entries get a default stat so `status` re-hashes the working-tree file rather than
//! trusting a stale cache. Like `checkout`/`restore`, paths from the tree are validated against
//! the checkout CVE class before they enter the index.

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::checkout::validate_path;
use crate::{Index, IndexEntry, Stat, WorkTree, WorktreeError};

pub(crate) async fn run<F, H>(wt: &WorkTree<F, H>, tree: ObjectId<H>) -> Result<(), WorktreeError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	let entries = wt.repository().read_tree(tree).await?;

	// Validate up front so a hostile tree path (`../x`, `.git/config`) cannot enter the index.
	for (path, _, _) in &entries {
		validate_path(path)?;
	}

	let mut index = Index::new();
	for (path, mode, oid) in entries {
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: u32::from_str_radix(&mode, 8).unwrap_or(0o100644),
			oid,
			stage: 0,
			assume_valid: false,
			path,
		});
	}
	wt.save_index(&index)
}
