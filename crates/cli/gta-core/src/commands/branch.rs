use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::{BranchName, Repository};

use crate::dispatch::{self, RepoCommand};
use crate::identity::CliIdentity;

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
	let name = BranchName::parse(name)?;
	repo
		.create_branch(&name, start, &CliIdentity::new(repo))
		.await?;
	Ok(())
}
