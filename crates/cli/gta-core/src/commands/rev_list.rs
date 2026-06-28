use std::path::Path;

use anyhow::Result;

use crate::repo;

/// List commits reachable from `spec`, newest first.
pub async fn run(cwd: &Path, spec: &str) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	let tip = repo.rev_parse(spec).await?;
	for oid in repo.rev_list(&[tip]).await? {
		println!("{oid}");
	}
	Ok(())
}
