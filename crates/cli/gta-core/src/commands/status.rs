use std::path::Path;

use anyhow::Result;

use crate::repo;

/// Print the working-tree status in `git status --porcelain=v1` form.
pub async fn run(cwd: &Path) -> Result<()> {
	let status = repo::open_worktree(cwd)?.status().await?;
	print!("{}", status.porcelain_v1());
	Ok(())
}
