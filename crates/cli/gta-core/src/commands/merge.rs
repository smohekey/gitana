use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::MergeOutcome;
use gitana_worktree::WorkTree;

use crate::commands::conflict;
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

/// Merge `commit` into the current branch, or carry an in-progress merge to its end.
///
/// Fast-forwards when the current branch is an ancestor of `commit` (unless `--no-ff`), otherwise
/// creates a true two-parent merge commit. `--ff-only` refuses a non-fast-forward. A merge that
/// conflicts materialises an in-progress state (`MERGE_HEAD`, `MERGE_MSG`, a conflicted index, and
/// work-tree markers) and exits non-zero; the user resolves it and then `--continue`s (or
/// `gta commit`s), or `--abort`s to discard it.
pub async fn run(
	cwd: &Path,
	commit: Option<String>,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	abort: bool,
	continue_: bool,
) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		Merge {
			commit,
			message,
			no_ff,
			ff_only,
			abort,
			continue_,
		},
	)
	.await
}

struct Merge {
	commit: Option<String>,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	abort: bool,
	continue_: bool,
}

impl WorkTreeCommand for Merge {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return gitana_porcelain::abort_merge(&wt).await;
		}
		let identity = CliIdentity::new(wt.repository());
		if self.continue_ {
			let commit = gitana_porcelain::continue_merge(&wt, None, &identity).await?;
			println!("{commit}");
			return Ok(());
		}

		let Some(commit) = self.commit else {
			bail!("merge requires a commit (or --abort/--continue)");
		};
		let outcome = gitana_porcelain::merge(
			&wt,
			&commit,
			self.message,
			self.no_ff,
			self.ff_only,
			&identity,
		)
		.await?;
		render(outcome)
	}
}

/// Render a merge outcome to stdout, or turn a conflict into the process's exit. Shared with `pull`,
/// which integrates the fetched upstream via the same merge.
pub(crate) fn render<H: HashAlgorithm>(outcome: MergeOutcome<H>) -> Result<()> {
	match outcome {
		MergeOutcome::AlreadyUpToDate => println!("Already up to date."),
		MergeOutcome::FastForward { from, to } => match from {
			Some(from) => println!("Updating {}..{}\nFast-forward", short(from), short(to)),
			None => println!("Fast-forward"),
		},
		MergeOutcome::Made { .. } => println!("Merge made by the 'recursive' strategy."),
		MergeOutcome::Conflict { paths } => return Err(conflict::report_conflicts(&paths)),
	}
	Ok(())
}

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}
