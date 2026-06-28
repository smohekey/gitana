use std::path::PathBuf;

use anyhow::Result;

use crate::repo;

/// Create a git-compatible sha256 repository under `target/.git`.
pub async fn run(target: PathBuf) -> Result<()> {
	let git_dir = target.join(".git");
	// The engine writes config + HEAD; the local profile creates the directory
	// skeleton git requires (objects/, refs/, info/).
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}
	repo::open(&git_dir).init().await?;
	println!(
		"Initialized empty Gitana repository in {}",
		git_dir.display()
	);
	Ok(())
}
