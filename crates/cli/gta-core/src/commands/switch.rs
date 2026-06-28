use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// Switch the working tree and `HEAD` to branch `name`. With `create`, make the
/// branch (at `start`, default `HEAD`) first. With `force`, overwrite local changes.
pub async fn run(
	cwd: &Path,
	name: &str,
	create: bool,
	start: Option<String>,
	force: bool,
) -> Result<()> {
	let wt = repo::open_worktree(cwd)?;
	let repo = wt.repository();
	let branch = format!("refs/heads/{name}");

	if create {
		if repo.refs().resolve(&branch).await?.is_some() {
			bail!("a branch named '{name}' already exists");
		}
		let target = repo.rev_parse(start.as_deref().unwrap_or("HEAD")).await?;
		repo.refs().update_ref(&branch, target, None).await?;
	}

	let Some(commit) = repo.refs().resolve(&branch).await? else {
		bail!("invalid reference: {name}");
	};
	let tree = repo.commit_tree(commit).await?;
	wt.checkout(tree, force).await?;
	repo.refs().set_head_symbolic(&branch).await?;
	eprintln!("Switched to branch '{name}'");
	Ok(())
}
