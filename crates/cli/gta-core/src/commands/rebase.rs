//! `gta rebase` — replay the current branch's commits onto a new base.

use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::RebaseOutcome;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

/// Rebase the current branch onto `upstream` (or `--onto <newbase>`), or carry an in-progress rebase
/// to its end.
///
/// Replays the branch's commits that are not in `upstream`, oldest-first, as fresh cherry-picks on
/// the new base. A conflict stops the rebase with a materialised conflict (the `REBASE_*` state, a
/// conflicted index, work-tree markers); resolve it and `--continue`, drop the commit with `--skip`,
/// or restore the original branch with `--abort`. Linear histories only (a merge commit in the range
/// is refused); commits that become empty are dropped, while originally-empty commits are kept.
pub async fn run(
	cwd: &Path,
	upstream: Option<String>,
	onto: Option<String>,
	abort: bool,
	continue_: bool,
	skip: bool,
) -> Result<()> {
	if [abort, continue_, skip].iter().filter(|&&f| f).count() > 1 {
		bail!("--abort, --continue, and --skip are mutually exclusive");
	}
	dispatch::on_worktree(
		cwd,
		Rebase {
			upstream,
			onto,
			abort,
			continue_,
			skip,
		},
	)
	.await
}

struct Rebase {
	upstream: Option<String>,
	onto: Option<String>,
	abort: bool,
	continue_: bool,
	skip: bool,
}

impl WorkTreeCommand for Rebase {
	async fn run<H: HashAlgorithm>(
		self,
		wt: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: String,
	) -> Result<()> {
		let identity = CliIdentity::new(wt.repository());
		if self.abort {
			return gitana_porcelain::abort_rebase(&wt, &identity).await;
		}
		let outcome = if self.continue_ {
			gitana_porcelain::continue_rebase(&wt, &identity).await?
		} else if self.skip {
			gitana_porcelain::skip_rebase(&wt, &identity).await?
		} else {
			gitana_porcelain::rebase(&wt, self.upstream, self.onto, &identity).await?
		};
		render(outcome)
	}
}

/// Render a rebase outcome to stdout, or turn a conflict into the process's exit.
fn render<H: HashAlgorithm>(outcome: RebaseOutcome<H>) -> Result<()> {
	match outcome {
		RebaseOutcome::UpToDate { branch } => {
			println!("Current branch {} is up to date.", branch_short(&branch));
		}
		RebaseOutcome::FastForwarded { branch, onto } => {
			println!(
				"Fast-forwarded {} to {}.",
				branch_short(&branch),
				short(onto)
			);
		}
		RebaseOutcome::Rebased { branch } => {
			println!(
				"Successfully rebased and updated {}.",
				branch_short(&branch)
			);
		}
		RebaseOutcome::Conflict {
			commit,
			subject,
			paths,
		} => {
			for path in &paths {
				println!("CONFLICT (content): Merge conflict in {path}");
			}
			println!("could not apply {} {}", short(commit), subject);
			println!(
				"hint: resolve the conflicts, `gta add` them, then run `gta rebase --continue` (or --skip / --abort)"
			);
			return Err(crate::MergeConflict.into());
		}
	}
	Ok(())
}

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}

fn branch_short(branch: &str) -> &str {
	branch.strip_prefix("refs/heads/").unwrap_or(branch)
}
