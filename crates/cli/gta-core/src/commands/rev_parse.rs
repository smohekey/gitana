use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// Resolve a revision spec to an object id.
pub async fn run(cwd: &Path, spec: &[u8]) -> Result<()> {
	dispatch::on_repo(cwd, RevParse { spec }).await
}

struct RevParse<'a> {
	spec: &'a [u8],
}

impl RepoCommand for RevParse<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let oid = repo.rev_parse(self.spec).await?;
		println!("{oid}");
		Ok(())
	}
}
