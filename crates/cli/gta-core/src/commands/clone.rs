//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gitana_git_http::parse_advertisement;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_remote::{self as transport, Origin};
use gitana_worktree::WorkTree;

use crate::repo;

/// Clone the repository at `url` into `dir` (default: the repo slug). Anonymous: works
/// for public repos. The local repository is created in whatever object format the
/// remote advertises.
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

	// Negotiate the remote's object format before creating anything locally.
	let body = transport::fetch_advertisement(&origin, "git-upload-pack").await?;
	let kind = transport::negotiated_kind(&body)?;

	// Create the git directory skeleton, like `init`.
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

	match kind {
		HashKind::Sha1 => clone_into::<Sha1>(&origin, &git_dir, &target, &body).await?,
		HashKind::Sha256 => clone_into::<Sha256>(&origin, &git_dir, &target, &body).await?,
	}

	println!("Cloned '{}' into '{}'", origin.url, target.display());
	Ok(())
}

/// Initialise the repository under `H`, download every advertised tip, recreate the refs
/// and `HEAD`, and populate the working tree.
async fn clone_into<H: HashAlgorithm>(
	origin: &Origin,
	git_dir: &Path,
	target: &Path,
	body: &[u8],
) -> Result<()> {
	// A freshly cloned repository is an ordinary checkout: its per-worktree and common dirs coincide.
	let repository = repo::open_generic::<H>(git_dir, git_dir);
	repository.init().await?; // writes a config matching H

	let advertised = parse_advertisement::<H>(body)?;
	let wants = transport::advertised_oids(&advertised);
	transport::fetch_pack(origin, &repository, &wants, &[]).await?;

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
	origin.save(git_dir)?;

	// Populate the working tree from HEAD (if the repo had any commits).
	if let Some(commit) = repository.refs().resolve_head().await? {
		let tree = repository.commit_tree(commit).await?;
		let worktree = WorkTree::new(repository, target, git_dir);
		worktree.checkout(tree, true).await?;
	}
	Ok(())
}
