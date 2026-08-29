use std::path::Path;

use crate::{Backend, ResultPathMode};
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_path::GitPathspec;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Move or rename tracked paths: filesystem move plus index update.
///
/// The last path is the destination; the rest are sources. `force` overwrites an existing
/// destination, and `dry_run` reports the moves without performing them. With `verbose` (or
/// `dry_run`), prints `Renaming <src> to <dst>` for each move, as git does.
pub async fn run(
	cwd: &Path,
	force: bool,
	dry_run: bool,
	verbose: bool,
	paths: Vec<GitPathspec>,
	result_path_mode: ResultPathMode,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Mv {
			force,
			dry_run,
			verbose,
			paths,
			result_path_mode,
		},
	)
	.await
}

struct Mv {
	force: bool,
	dry_run: bool,
	verbose: bool,
	paths: Vec<GitPathspec>,
	result_path_mode: ResultPathMode,
}

impl WorkTreeCommand for Mv {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		if self.paths.len() < 2 {
			bail!("must specify at least one source and a destination");
		}
		let (dest, sources) = self.paths.split_last().unwrap();

		let moves = worktree
			.mv_pathspecs(sources, dest, &prefix, self.force, self.dry_run)
			.await?;

		if self.verbose || self.dry_run {
			for (from, to) in &moves {
				let from = crate::git_path::render_result_path(from, self.result_path_mode);
				let to = crate::git_path::render_result_path(to, self.result_path_mode);
				println!("Renaming {from} to {to}");
			}
		}
		Ok(())
	}
}
