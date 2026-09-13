use std::path::Path;

use crate::{Backend, ResultPathMode};
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_path::GitPathspec;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Remove tracked paths from the index and (unless `cached`) the working tree.
///
/// `force` overrides the data-safety check, `recursive` allows removing a directory's tracked
/// contents, and `dry_run` reports what would be removed without changing anything. Prints
/// `rm '<path>'` for each removed path, as git does.
pub async fn run(
	cwd: &Path,
	cached: bool,
	force: bool,
	recursive: bool,
	dry_run: bool,
	pathspecs: Vec<GitPathspec>,
	result_path_mode: ResultPathMode,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Rm {
			cached,
			force,
			recursive,
			dry_run,
			pathspecs,
			result_path_mode,
		},
	)
	.await
}

struct Rm {
	cached: bool,
	force: bool,
	recursive: bool,
	dry_run: bool,
	pathspecs: Vec<GitPathspec>,
	result_path_mode: ResultPathMode,
}

impl WorkTreeCommand for Rm {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		if self.pathspecs.is_empty() {
			bail!("no pathspec given");
		}
		let outcome = worktree
			.rm_pathspecs(
				&self.pathspecs,
				&prefix,
				self.cached,
				self.force,
				self.recursive,
				self.dry_run,
			)
			.await?;
		// Report the removals that did happen first, then surface a per-path failure — so the side
		// effects are visible even when a later path could not be removed.
		for path in &outcome.removed {
			let path = crate::git_path::render_result_path(path, self.result_path_mode);
			println!("rm '{path}'");
		}
		if let Some(error) = outcome.failure {
			return Err(error.into());
		}
		Ok(())
	}
}
