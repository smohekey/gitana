//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_worktree::WorkTree;

use crate::repo;
use crate::transport::{self, Origin, advertised_oids};

/// Clone the repository at `url` into `dir` (default: the repo slug). Anonymous: works
/// for public repos.
pub async fn run(url: String, dir: Option<PathBuf>) -> Result<()> {
	let origin = Origin::parse(&url)?;
	let target = dir.unwrap_or_else(|| PathBuf::from(origin.directory_name()));
	if target.exists()
		&& target
			.read_dir()
			.map(|mut entries| entries.next().is_some())
			.unwrap_or(false)
	{
		bail!(
			"destination path '{}' already exists and is not empty",
			target.display()
		);
	}

	// Create the git directory skeleton and metadata, like `init`.
	let git_dir = target.join(".git");
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}
	let repository = repo::open(&git_dir);
	repository.init().await?;

	// Discover refs, then download every advertised tip.
	let advertised = transport::discover_upload(&origin).await?;
	let wants = advertised_oids(&advertised);
	transport::fetch_pack(&origin, &repository, &wants, &[]).await?;

	// Recreate the refs and HEAD locally.
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/") {
			repository.refs().update_ref(name, *oid, None).await?;
		}
	}
	let head_target = advertised
		.head_target
		.clone()
		.unwrap_or_else(|| "refs/heads/main".to_owned());
	repository.refs().set_head_symbolic(&head_target).await?;
	origin.save(&git_dir)?;

	// Populate the working tree from HEAD (if the repo had any commits).
	if let Some(commit) = repository.refs().resolve_head().await? {
		let tree = repository.commit_tree(commit).await?;
		let worktree = WorkTree::new(repository, &target, &git_dir);
		worktree.checkout(tree, true).await?;
	}

	println!("Cloned '{}' into '{}'", origin.url, target.display());
	Ok(())
}
