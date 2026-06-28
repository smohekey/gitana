use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// List branches, or create `name` at `start` (default `HEAD`).
pub async fn run(cwd: &Path, name: Option<String>, start: Option<String>) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	match name {
		None => list(&repo).await,
		Some(name) => create(&repo, &name, start.as_deref()).await,
	}
}

async fn list(repo: &repo::LocalRepository) -> Result<()> {
	let current = repo
		.refs()
		.read_symbolic("HEAD")
		.await?
		.and_then(|t| t.strip_prefix("refs/heads/").map(str::to_owned));
	for (name, _) in repo.refs().list("refs/heads/").await? {
		let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
		let marker = if Some(short) == current.as_deref() {
			"* "
		} else {
			"  "
		};
		println!("{marker}{short}");
	}
	Ok(())
}

async fn create(repo: &repo::LocalRepository, name: &str, start: Option<&str>) -> Result<()> {
	let full = format!("refs/heads/{name}");
	if repo.refs().resolve(&full).await?.is_some() {
		bail!("a branch named '{name}' already exists");
	}
	let target = repo.rev_parse(start.unwrap_or("HEAD")).await?;
	repo.refs().update_ref(&full, target, None).await?;
	Ok(())
}
