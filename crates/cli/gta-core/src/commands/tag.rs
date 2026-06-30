use std::path::Path;

use anyhow::{Result, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// List tags, or create a lightweight tag `name` at `target` (default `HEAD`).
pub async fn run(cwd: &Path, name: Option<String>, target: Option<String>) -> Result<()> {
	dispatch::on_repo(cwd, Tag { name, target }).await
}

struct Tag {
	name: Option<String>,
	target: Option<String>,
}

impl RepoCommand for Tag {
	async fn run<H: HashAlgorithm>(self, repo: Repository<LocalFileStore, H>) -> Result<()> {
		match self.name {
			None => {
				for (name, _) in repo.refs().list("refs/tags/").await? {
					println!("{}", name.strip_prefix("refs/tags/").unwrap_or(&name));
				}
				Ok(())
			}
			Some(name) => {
				let full = format!("refs/tags/{name}");
				if repo.refs().resolve(&full).await?.is_some() {
					bail!("tag '{name}' already exists");
				}
				let oid = repo
					.rev_parse(self.target.as_deref().unwrap_or("HEAD"))
					.await?;
				repo.refs().update_ref(&full, oid, None).await?;
				Ok(())
			}
		}
	}
}
