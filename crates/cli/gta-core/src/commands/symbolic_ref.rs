use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::{ReflogIntent, Repository};

use crate::dispatch::{self, RepoCommand};
use crate::identity::signature_or_default;

/// Read or set a symbolic ref (`name [target]`).
pub async fn run(cwd: &Path, name: &str, target: Option<String>) -> Result<()> {
	dispatch::on_repo(cwd, SymbolicRef { name, target }).await
}

struct SymbolicRef<'a> {
	name: &'a str,
	target: Option<String>,
}

impl RepoCommand for SymbolicRef<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		match self.target {
			Some(target) => {
				// git records an empty-message reflog entry when retargeting a logged symref (e.g. HEAD).
				let committer = signature_or_default(&repo, "COMMITTER").await;
				repo
					.refs()
					.set_symbolic(
						self.name,
						&target,
						ReflogIntent::Log {
							committer: &committer,
							message: "",
						},
					)
					.await?
			}
			None => match repo.refs().read_symbolic(self.name).await? {
				Some(target) => println!("{target}"),
				None => bail!("{} is not a symbolic ref", self.name),
			},
		}
		Ok(())
	}
}
