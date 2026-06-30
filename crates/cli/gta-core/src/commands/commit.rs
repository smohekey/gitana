use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::{FileMode, TreeBuildEntry};
use gitana_worktree::{Index, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

/// Create a commit from the index on the current branch.
pub async fn run(cwd: &Path, message: &str) -> Result<()> {
	dispatch::on_worktree(cwd, Commit { message }).await
}

struct Commit<'a> {
	message: &'a str,
}

impl WorkTreeCommand for Commit<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		_prefix: String,
	) -> Result<()> {
		let index = worktree.load_index()?;
		// An unmerged index would otherwise silently drop conflicted paths (they have no stage-0 entry)
		// from the tree, so refuse — as git does — until they are resolved.
		if index.has_conflicts() {
			bail!(
				"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
			);
		}

		let repo = worktree.repository();
		// Concluding a merge: produce a two-parent merge commit (and clear `MERGE_HEAD`), so resolving
		// and `gta commit` does not silently drop the merge's second parent.
		if repo.merge_head().await?.is_some() {
			return crate::commands::merge::complete_merge(&worktree, Some(self.message.to_owned()))
				.await;
		}
		// Concluding a cherry-pick: a single-parent commit preserving the picked author (clears
		// `CHERRY_PICK_HEAD`).
		if repo.cherry_pick_head().await?.is_some() {
			return crate::commands::cherry_pick::complete(&worktree, Some(self.message.to_owned()))
				.await;
		}
		// Concluding a revert: a single-parent commit authored by the current user (clears `REVERT_HEAD`).
		if repo.revert_head().await?.is_some() {
			return crate::commands::revert::complete(&worktree, Some(self.message.to_owned())).await;
		}

		let entries = index_tree_entries(&index);
		if entries.is_empty() {
			bail!("nothing to commit (empty index)");
		}

		let tree = repo.write_tree(&entries).await?;
		let author = identity::signature(repo, "AUTHOR").await?;
		let committer = identity::signature(repo, "COMMITTER").await?;
		let message = if self.message.ends_with('\n') {
			self.message.to_owned()
		} else {
			format!("{}\n", self.message)
		};
		let commit = repo
			.commit_on_head(tree, &author, &committer, &message)
			.await?;
		println!("{commit}");
		Ok(())
	}
}

/// The stage-0 index entries as tree-build entries — the content a commit captures.
pub(crate) fn index_tree_entries<H: HashAlgorithm>(index: &Index<H>) -> Vec<TreeBuildEntry<H>> {
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
