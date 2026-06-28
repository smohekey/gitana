//! `gta pull` — fetch the current branch from the origin, fast-forward it, and update
//! the working tree. Merge (non-fast-forward) is not supported.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_repository::HeadState;
use gitana_worktree::WorkTree;

use crate::repo;
use crate::transport::{self, Origin, local_haves};

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	let (work, git_dir) = repo::discover(cwd)?;
	let repository = repo::open(&git_dir);
	let origin = Origin::load(&git_dir)?;

	let branch = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot pull onto a detached HEAD"),
	};

	let advertised = transport::discover_upload(&origin).await?;
	let remote_tip = advertised
		.oid_of(&branch)
		.with_context(|| format!("origin has no {branch}"))?;
	let local = repository.refs().resolve(&branch).await?;

	let haves = local_haves(&repository).await?;
	transport::fetch_pack(&origin, &repository, &[remote_tip], &haves).await?;

	match local {
		Some(old) if old == remote_tip => {
			println!("Already up to date.");
			return Ok(());
		}
		Some(old) => {
			if !repository.rev_list(&[remote_tip]).await?.contains(&old) {
				bail!("cannot fast-forward {branch}; merge is not supported");
			}
			repository
				.refs()
				.update_ref(&branch, remote_tip, Some(old))
				.await?;
		}
		None => {
			repository
				.refs()
				.update_ref(&branch, remote_tip, None)
				.await?;
		}
	}

	let tree = repository.commit_tree(remote_tip).await?;
	let worktree = WorkTree::new(repository, &work, &git_dir);
	worktree.checkout(tree, true).await?;

	println!("Updated {branch} -> {remote_tip}");
	Ok(())
}
