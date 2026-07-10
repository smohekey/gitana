use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::{ReflogIntent, Repository};

use crate::dispatch::{self, RepoCommand};
use crate::identity::signature_or_default;

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
		// git logs an update to a logged ref even without `-m`, recording an empty message (no tab).
		let committer = signature_or_default(&repo, "COMMITTER").await?;
		repo
			.refs()
			.update_ref(
				self.name,
				new,
				current,
				ReflogIntent::Log {
					committer: &committer,
					message: "",
				},
			)
			.await?;
		Ok(())
	}
}
