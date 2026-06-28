use std::path::Path;

use anyhow::Result;

use crate::repo;

/// Point `name` at the object `value` resolves to (creating the ref if absent).
pub async fn run(cwd: &Path, name: &str, value: &str) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	let new = repo.rev_parse(value).await?;
	let current = repo.refs().resolve(name).await?;
	repo.refs().update_ref(name, new, current).await?;
	Ok(())
}
