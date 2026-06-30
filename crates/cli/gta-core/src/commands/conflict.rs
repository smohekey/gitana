//! Shared mechanics for the conflict lifecycle of merge-like operations (merge and cherry-pick, and
//! later revert): materialising a conflicted work tree and index, reporting the conflicts as a typed
//! outcome, building the resolved tree, and restoring the work tree on abort. The operation-specific
//! state (`MERGE_HEAD` vs `CHERRY_PICK_HEAD`) and the shape of the concluding commit live in each
//! command.

use std::collections::HashMap;

use anyhow::{Result, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::commands::commit::index_tree_entries;

/// Write the merged result to the work tree (conflicted files carry markers) and record the conflict
/// stages (1/2/3 from base/ours/theirs) in the index. Refuses — before any caller records operation
/// state — if the checkout would clobber a touched local change.
pub(crate) async fn write_conflicted_state<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	ours_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	conflicts: &[String],
) -> Result<()> {
	wt.checkout(merged_tree, false).await?;

	let repository = wt.repository();
	let base = tree_entry_map(repository, base_tree).await?;
	let ours = tree_entry_map(repository, ours_tree).await?;
	let theirs = tree_entry_map(repository, theirs_tree).await?;
	let mut index = wt.load_index()?;
	for path in conflicts {
		index.record_conflict(
			path,
			base.get(path).copied(),
			ours.get(path).copied(),
			theirs.get(path).copied(),
		);
	}
	wt.save_index(&index)?;
	Ok(())
}

/// Report the conflicted paths on stdout and return the typed [`crate::MergeConflict`] outcome. The
/// front-end turns it into a non-zero exit (`gta`) or a tool error (`gta-mcp`); a library function
/// must not decide the process's fate with `exit`, which would terminate a long-lived MCP server.
pub(crate) fn report_conflicts(conflicts: &[String]) -> anyhow::Error {
	for path in conflicts {
		println!("CONFLICT (content): Merge conflict in {path}");
	}
	crate::MergeConflict.into()
}

/// The tree captured by the resolved index, refusing while unmerged stages remain. An empty index is
/// valid here (e.g. a delete/modify conflict resolved by deletion): `write_tree(&[])` is an empty
/// tree, unlike an ordinary commit which rejects it.
pub(crate) async fn resolved_tree<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
) -> Result<ObjectId<H>> {
	let index = wt.load_index()?;
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}
	let entries = index_tree_entries(&index);
	Ok(wt.repository().write_tree(&entries).await?)
}

/// The tree the index currently records (stage-0 entries only), assuming no unmerged stages. Used
/// to require a clean index before starting an operation (the index must equal `HEAD`).
pub(crate) async fn index_tree<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
) -> Result<ObjectId<H>> {
	let entries = index_tree_entries(&wt.load_index()?);
	Ok(wt.repository().write_tree(&entries).await?)
}

/// Restore the work tree and index to the (unmoved) `HEAD`, discarding conflict markers and unmerged
/// stages — the shared core of `--abort`. The caller clears its own operation state afterwards.
pub(crate) async fn restore_to_head<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(head) = repository.refs().resolve_head().await? else {
		bail!("HEAD is unborn");
	};
	let head_tree = repository.commit_tree(head).await?;
	wt.checkout(head_tree, true).await?;
	Ok(())
}

/// A tree's entries as `path -> (mode, oid)`, for recording conflict stages.
async fn tree_entry_map<H: HashAlgorithm>(
	repository: &Repository<LocalFileStore, H>,
	tree: ObjectId<H>,
) -> Result<HashMap<String, (u32, ObjectId<H>)>> {
	let mut map = HashMap::new();
	for (path, mode, oid) in repository.read_tree(tree).await? {
		let mode = u32::from_str_radix(&mode, 8).unwrap_or(0o100644);
		map.insert(path, (mode, oid));
	}
	Ok(map)
}

/// Ensure a commit message ends with a single trailing newline.
pub(crate) fn ensure_trailing_newline(message: String) -> String {
	if message.ends_with('\n') {
		message
	} else {
		format!("{message}\n")
	}
}
