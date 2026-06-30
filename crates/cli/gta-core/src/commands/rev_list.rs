use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// List commits reachable from `spec`, newest first.
pub async fn run(cwd: &Path, spec: &str) -> Result<()> {
	dispatch::on_repo(cwd, RevList { spec }).await
}

struct RevList<'a> {
	spec: &'a str,
}

impl RepoCommand for RevList<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let tip = repo.rev_parse(self.spec).await?;
		for oid in repo.rev_list(&[tip]).await? {
			println!("{oid}");
		}
		Ok(())
	}
}
