use std::path::Path;

use anyhow::Result;
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// Resolve a revision spec to an object id.
pub async fn run(cwd: &Path, spec: &str) -> Result<()> {
	dispatch::on_repo(cwd, RevParse { spec }).await
}

struct RevParse<'a> {
	spec: &'a str,
}

impl RepoCommand for RevParse<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<LocalFileStore, H>) -> Result<()> {
		let oid = repo.rev_parse(self.spec).await?;
		println!("{oid}");
		Ok(())
	}
}
