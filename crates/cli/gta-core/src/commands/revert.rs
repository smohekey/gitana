//! `gta revert` — record a new commit that undoes a previous commit's change.

use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_porcelain::RevertOutcome;
use gitana_worktree::WorkTree;

use crate::commands::conflict;
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

/// Revert `commit` on the current branch, or carry an in-progress revert to its end.
///
/// Records a new single-parent commit that undoes the change `commit` introduced — a three-way merge
/// of `commit`, `HEAD`, and `commit`'s parent — authored by the current user. A conflict materialises
/// an in-progress state (`REVERT_HEAD`, `MERGE_MSG`, a conflicted index, work-tree markers) and exits
/// non-zero; resolve it and `--continue` (or `gta commit`), or `--abort` to discard it.
pub async fn run(cwd: &Path, commit: Option<String>, abort: bool, continue_: bool) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		Revert {
			commit,
			abort,
			continue_,
		},
	)
	.await
}

struct Revert {
	commit: Option<String>,
	abort: bool,
	continue_: bool,
}

impl WorkTreeCommand for Revert {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return gitana_porcelain::abort_revert(&wt).await;
		}
		let identity = CliIdentity::new(wt.repository());
		if self.continue_ {
			let commit = gitana_porcelain::continue_revert(&wt, None, &identity).await?;
			println!("{commit}");
			return Ok(());
		}

		let Some(commit) = self.commit else {
			bail!("revert requires a commit (or --abort/--continue)");
		};
		match gitana_porcelain::revert(&wt, &commit, &identity).await? {
			RevertOutcome::Reverted { commit } => println!("{commit}"),
			RevertOutcome::Conflict { paths } => return Err(conflict::report_conflicts(&paths)),
		}
		Ok(())
	}
}
