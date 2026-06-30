use std::path::Path;

use anyhow::{Result, bail};
use gitana_repository::{FileMode, TreeBuildEntry};
use gitana_worktree::Index;

use crate::identity;
use crate::repo;

/// Create a commit from the index on the current branch.
pub async fn run(cwd: &Path, message: &str) -> Result<()> {
	let wt = repo::open_worktree(cwd)?;
	let index = wt.load_index()?;
	// An unmerged index would otherwise silently drop conflicted paths (they have no stage-0 entry)
	// from the tree, so refuse — as git does — until they are resolved.
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}

	let repo = wt.repository();
	// Concluding a merge: produce a two-parent merge commit (and clear `MERGE_HEAD`), so resolving
	// and `gta commit` does not silently drop the merge's second parent.
	if repo.merge_head().await?.is_some() {
		return crate::commands::merge::complete_merge(&wt, Some(message.to_owned())).await;
	}

	let entries = index_tree_entries(&index);
	if entries.is_empty() {
		bail!("nothing to commit (empty index)");
	}

	let tree = repo.write_tree(&entries).await?;
	let author = identity::signature(repo, "AUTHOR").await?;
	let committer = identity::signature(repo, "COMMITTER").await?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	let commit = repo
		.commit_on_head(tree, &author, &committer, &message)
		.await?;
	println!("{commit}");
	Ok(())
}

/// The stage-0 index entries as tree-build entries — the content a commit captures.
pub(crate) fn index_tree_entries(index: &Index) -> Vec<TreeBuildEntry> {
	index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| TreeBuildEntry {
			path: e.path.clone(),
			mode: file_mode(e.mode),
			id: e.oid,
		})
		.collect()
}

fn file_mode(mode: u32) -> FileMode {
	match mode {
		0o100755 => FileMode::Executable,
		0o120000 => FileMode::Symlink,
		_ => FileMode::Regular,
	}
}
