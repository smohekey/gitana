use std::path::Path;

use anyhow::Result;

use crate::repo;

/// Stage the given pathspecs (files, directories, or `.`), interpreted relative to `cwd`.
pub async fn run(cwd: &Path, pathspecs: &[String]) -> Result<()> {
	let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
	let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
	wt.add(&specs, &prefix).await?;
	Ok(())
}
