use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Print the working-tree status in `git status --porcelain=v1` form.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Status {
			cwd: cwd.to_owned(),
		},
	)
	.await
}

struct Status {
	cwd: PathBuf,
}

impl WorkTreeCommand for Status {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		// git consults its global excludes file (`core.excludesFile`) for untracked detection; it lives
		// outside the worktree, so resolve its content here and pass it in. `core.ignoreCase` and
		// `.git/info/exclude` are read inside the worktree crate.
		let config = worktree.repository().effective_config().await?;
		let excludes_file = crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;
		let status = worktree.status(excludes_file.as_deref()).await?;
		print!("{}", status.porcelain_v1());
		Ok(())
	}
}
