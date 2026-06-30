use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

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
		let repo = worktree.repository();
		// A rebase replays commits itself; a plain `gta commit` would create a stray commit the
		// sequencer doesn't track, so direct the user to `gta rebase --continue`.
		if repo.rebase_in_progress().await? {
			bail!(
				"you are in the middle of a rebase; run `gta rebase --continue` instead of `gta commit`"
			);
		}
		// Concluding an in-progress operation: each produces the right shape of commit and clears its
		// state. (These completions will move to porcelain with the history-editing cluster.)
		if repo.merge_head().await?.is_some() {
			return crate::commands::merge::complete_merge(&worktree, Some(self.message.to_owned()))
				.await;
		}
		if repo.cherry_pick_head().await?.is_some() {
			return crate::commands::cherry_pick::complete(&worktree, Some(self.message.to_owned()))
				.await;
		}
		if repo.revert_head().await?.is_some() {
			return crate::commands::revert::complete(&worktree, Some(self.message.to_owned())).await;
		}

		// Plain commit: the porcelain operation records the staged tree (refusing an unmerged or empty
		// index first), resolving the git identity only if a commit will actually be made.
		let id = gitana_porcelain::commit(&worktree, self.message, async || {
			let author = identity::signature(repo, "AUTHOR").await?;
			let committer = identity::signature(repo, "COMMITTER").await?;
			Ok((author, committer))
		})
		.await?;
		println!("{id}");
		Ok(())
	}
}
