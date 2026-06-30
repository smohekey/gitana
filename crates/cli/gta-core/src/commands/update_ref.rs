use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// Point `name` at the object `value` resolves to (creating the ref if absent).
pub async fn run(cwd: &Path, name: &str, value: &str) -> Result<()> {
	dispatch::on_repo(cwd, UpdateRef { name, value }).await
}

struct UpdateRef<'a> {
	name: &'a str,
	value: &'a str,
}

impl RepoCommand for UpdateRef<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let new = repo.rev_parse(self.value).await?;
		let current = repo.refs().resolve(self.name).await?;
		repo.refs().update_ref(self.name, new, current).await?;
		Ok(())
	}
}
