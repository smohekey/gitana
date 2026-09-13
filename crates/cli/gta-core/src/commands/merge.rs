use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use cap_std::fs::Dir;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::MergeOutcome;
use gitana_worktree::WorkTree;

use crate::commands::conflict;
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;
use crate::signer;

/// Merge `commit` into the current branch, or carry an in-progress merge to its end.
///
/// Fast-forwards when the current branch is an ancestor of `commit` (unless `--no-ff`), otherwise
/// creates a true two-parent merge commit. `--ff-only` refuses a non-fast-forward. A merge that
/// conflicts materialises an in-progress state (`MERGE_HEAD`, `MERGE_MSG`, a conflicted index, and
/// work-tree markers) and exits non-zero; the user resolves it and then `--continue`s (or
/// `gta commit`s), or `--abort`s to discard it.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	commit: Option<String>,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	abort: bool,
	continue_: bool,
	result_path_mode: crate::ResultPathMode,
) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree_history_mutation(
		cwd,
		Merge {
			commit,
			message,
			no_ff,
			ff_only,
			abort,
			continue_,
			result_path_mode,
			cwd: cwd.to_path_buf(),
			cwd_directory: None,
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
	result_path_mode: crate::ResultPathMode,
	/// The effective working directory, for resolving a relative `user.signingkey` (`-C`).
	cwd: std::path::PathBuf,
	cwd_directory: Option<Dir>,
}

impl WorkTreeCommand for Merge {
	fn set_command_directory(&mut self, path: std::path::PathBuf, directory: Dir) {
		self.cwd = path;
		self.cwd_directory = Some(directory);
	}

	async fn run<H: HashAlgorithm>(
		self,
		wt: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: gitana_path::GitPath,
	) -> Result<()> {
		if self.abort {
			return gitana_porcelain::abort_merge(&wt).await;
		}
		let identity = CliIdentity::new(wt.repository());
		// The merge commit is signed when git config requests it (`commit.gpgsign` + `gpg.format=ssh`).
		let signer = signer::config_signer_in(
			wt.repository(),
			&self.cwd,
			self
				.cwd_directory
				.as_ref()
				.expect("dispatch retained the command directory"),
		)
		.await?;
		if self.continue_ {
			let commit = gitana_porcelain::continue_merge(&wt, None, &identity, signer.as_ref()).await?;
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
			signer.as_ref(),
		)
		.await?;
		render(outcome, self.result_path_mode)
	}
}

/// Render a merge outcome to stdout, or turn a conflict into the process's exit. Shared with `pull`,
/// which integrates the fetched upstream via the same merge.
pub(crate) fn render<H: HashAlgorithm>(
	outcome: MergeOutcome<H>,
	result_path_mode: crate::ResultPathMode,
) -> Result<()> {
	match outcome {
		MergeOutcome::AlreadyUpToDate => println!("Already up to date."),
		MergeOutcome::FastForward { from, to } => match from {
			Some(from) => println!("Updating {}..{}\nFast-forward", short(from), short(to)),
			None => println!("Fast-forward"),
		},
		MergeOutcome::Made { .. } => println!("Merge made by the 'recursive' strategy."),
		MergeOutcome::WouldOverwrite { paths } => {
			bail!("{}", would_overwrite_message(&paths, result_path_mode));
		}
		MergeOutcome::Conflict { paths } => {
			return Err(conflict::report_conflicts(&paths, result_path_mode));
		}
	}
	Ok(())
}

fn would_overwrite_message(paths: &[gitana_path::GitPath], mode: crate::ResultPathMode) -> String {
	let paths = paths
		.iter()
		.map(|path| crate::git_path::render_result_path(path, mode))
		.collect::<Vec<_>>()
		.join("\n  ");
	format!(
		"Your local changes to the following files would be overwritten by merge:\n  {paths}\nPlease commit your changes or stash them before you merge."
	)
}

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reversible_merge_refusal_distinguishes_human_path_collisions() {
		let raw = gitana_path::GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = gitana_path::GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let paths = [raw, literal];
		let human = would_overwrite_message(&paths, crate::ResultPathMode::Human);
		let reversible = would_overwrite_message(&paths, crate::ResultPathMode::Reversible);

		assert_eq!(human.matches("  \"raw-\\377\"").count(), 2);
		assert_eq!(reversible.matches("  \"raw-\\377\"").count(), 1);
		assert!(reversible.contains("  \"\\\"raw-\\\\377\\\"\""));
	}
}
