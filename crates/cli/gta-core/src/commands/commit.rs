use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

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
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: String,
	) -> Result<()> {
		let repo = worktree.repository();
		let identity = CliIdentity::new(repo);
		// A rebase replays commits itself; a plain `gta commit` would create a stray commit the
		// sequencer doesn't track, so direct the user to `gta rebase --continue`.
		if repo.rebase_in_progress().await? {
			bail!(
				"you are in the middle of a rebase; run `gta rebase --continue` instead of `gta commit`"
			);
		}
		// Concluding an in-progress operation: each porcelain `continue_*` produces the right shape of
		// commit (two-parent merge / author-preserving pick / reverter-authored revert) and clears its
		// state.
		if repo.merge_head().await?.is_some() {
			let commit =
				gitana_porcelain::continue_merge(&worktree, Some(self.message.to_owned()), &identity)
					.await?;
			println!("{commit}");
			return Ok(());
		}
		if repo.cherry_pick_head().await?.is_some() {
			let commit =
				gitana_porcelain::continue_cherry_pick(&worktree, Some(self.message.to_owned()), &identity)
					.await?;
			println!("{commit}");
			return Ok(());
		}
		if repo.revert_head().await?.is_some() {
			let commit =
				gitana_porcelain::continue_revert(&worktree, Some(self.message.to_owned()), &identity)
					.await?;
			println!("{commit}");
			return Ok(());
		}

		// Plain commit: the porcelain operation records the staged tree (refusing an unmerged or empty
		// index first), resolving the git identity only if a commit will actually be made.
		let id = gitana_porcelain::commit(&worktree, self.message, &identity).await?;
		println!("{id}");
		Ok(())
	}
}
