use std::path::Path;

use anyhow::{Result, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// Read or set a symbolic ref (`name [target]`).
pub async fn run(cwd: &Path, name: &str, target: Option<String>) -> Result<()> {
	dispatch::on_repo(cwd, SymbolicRef { name, target }).await
}

struct SymbolicRef<'a> {
	name: &'a str,
	target: Option<String>,
}

impl RepoCommand for SymbolicRef<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<LocalFileStore, H>) -> Result<()> {
		match self.target {
			Some(target) => repo.refs().set_symbolic(self.name, &target).await?,
			None => match repo.refs().read_symbolic(self.name).await? {
				Some(target) => println!("{target}"),
				None => bail!("{} is not a symbolic ref", self.name),
			},
		}
		Ok(())
	}
}
