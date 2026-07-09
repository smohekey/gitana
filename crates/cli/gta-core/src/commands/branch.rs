use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::{ReflogIntent, Repository};

use crate::dispatch::{self, RepoCommand};
use crate::identity::signature_or_default;

/// List branches, or create `name` at `start` (default `HEAD`).
pub async fn run(cwd: &Path, name: Option<String>, start: Option<String>) -> Result<()> {
	dispatch::on_repo(cwd, Branch { name, start }).await
}

struct Branch {
	name: Option<String>,
	start: Option<String>,
}

impl RepoCommand for Branch {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		match self.name {
			None => list(&repo).await,
			Some(name) => create(&repo, &name, self.start.as_deref()).await,
		}
	}
}

async fn list<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<()> {
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

async fn create<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	name: &str,
	start: Option<&str>,
) -> Result<()> {
	let full = format!("refs/heads/{name}");
	if repo.refs().resolve(&full).await?.is_some() {
		bail!("a branch named '{name}' already exists");
	}
	let target = repo.rev_parse(start.unwrap_or("HEAD")).await?;
	// git's reflog records the start point as the user named it, defaulting to the current branch's
	// short name (its full object id when HEAD is detached) when none was given.
	let from = match start {
		Some(start) => start.to_owned(),
		None => start_from_head(repo).await?,
	};
	let committer = signature_or_default(repo, "COMMITTER").await;
	let message = format!("branch: Created from {from}");
	repo
		.refs()
		.update_ref(
			&full,
			target,
			None,
			ReflogIntent::Log {
				committer: &committer,
				message: &message,
			},
		)
		.await?;
	Ok(())
}

/// The reflog "Created from" description of `HEAD` when no start point is named: the current branch's
/// short name, or the literal `HEAD` when it is detached (git records `HEAD`, not the object id,
/// for a branch created off a detached HEAD).
async fn start_from_head<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<String> {
	match repo.refs().read_symbolic("HEAD").await? {
		Some(target) => Ok(
			target
				.strip_prefix("refs/heads/")
				.unwrap_or(&target)
				.to_owned(),
		),
		None => Ok("HEAD".to_owned()),
	}
}
