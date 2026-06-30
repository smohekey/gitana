use std::path::Path;

use anyhow::Result;
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Print the working-tree status in `git status --porcelain=v1` form.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_worktree(cwd, Status).await
}

struct Status;

impl WorkTreeCommand for Status {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<LocalFileStore, H>,
		_prefix: String,
	) -> Result<()> {
		let status = worktree.status().await?;
		print!("{}", status.porcelain_v1());
		Ok(())
	}
}
