use std::path::Path;

use anyhow::Result;

use crate::repo;

/// Resolve a revision spec to an object id.
pub async fn run(cwd: &Path, spec: &str) -> Result<()> {
	let oid = repo::open_here(cwd)?.rev_parse(spec).await?;
	println!("{oid}");
	Ok(())
}
