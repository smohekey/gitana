use std::path::Path;

use anyhow::Result;

use crate::repo;

/// Stage the given pathspecs (files, directories, or `.`).
pub async fn run(cwd: &Path, pathspecs: &[String]) -> Result<()> {
	let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
	repo::open_worktree(cwd)?.add(&specs).await?;
	Ok(())
}
