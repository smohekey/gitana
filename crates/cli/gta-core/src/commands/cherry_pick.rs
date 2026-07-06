//! `gta cherry-pick` — re-apply a commit's change onto the current branch.

use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_porcelain::PickOutcome;
use gitana_worktree::WorkTree;

use crate::commands::conflict;
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;
use crate::signer;

/// Cherry-pick `commit` onto the current branch, or carry an in-progress cherry-pick to its end.
///
/// Re-applies the change `commit` introduced — a three-way merge of its parent, `HEAD`, and `commit`
/// — as a new single-parent commit that preserves `commit`'s author. A conflict materialises an
/// in-progress state (`CHERRY_PICK_HEAD`, `MERGE_MSG`, a conflicted index, work-tree markers) and
/// exits non-zero; resolve it and `--continue` (or `gta commit`), or `--abort` to discard it.
pub async fn run(cwd: &Path, commit: Option<String>, abort: bool, continue_: bool) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		CherryPick {
			commit,
			abort,
			continue_,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
}

struct CherryPick {
	commit: Option<String>,
	abort: bool,
	continue_: bool,
	/// The effective working directory, for resolving a relative `user.signingkey` (`-C`).
	cwd: std::path::PathBuf,
}

impl WorkTreeCommand for CherryPick {
	async fn run<H: HashAlgorithm>(
		self,
		wt: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: String,
	) -> Result<()> {
		if self.abort {
			return gitana_porcelain::abort_cherry_pick(&wt).await;
		}
		let identity = CliIdentity::new(wt.repository());
		// The picked commit is signed when git config requests it (`commit.gpgsign` + `gpg.format=ssh`).
		let signer = signer::config_signer(wt.repository(), &self.cwd).await?;
		if self.continue_ {
			let commit =
				gitana_porcelain::continue_cherry_pick(&wt, None, &identity, signer.as_ref()).await?;
			println!("{commit}");
			return Ok(());
		}

		let Some(commit) = self.commit else {
			bail!("cherry-pick requires a commit (or --abort/--continue)");
		};
		match gitana_porcelain::cherry_pick(&wt, &commit, &identity, signer.as_ref()).await? {
			PickOutcome::Picked { commit } => println!("{commit}"),
			PickOutcome::Conflict { paths } => return Err(conflict::report_conflicts(&paths)),
		}
		Ok(())
	}
}
