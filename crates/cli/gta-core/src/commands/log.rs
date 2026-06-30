use std::path::Path;

use anyhow::Result;
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, parse_commit};
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// Print the commit history of `HEAD`, newest first, one line each.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_repo(cwd, Log).await
}

struct Log;

impl RepoCommand for Log {
	async fn run<H: HashAlgorithm>(self, repo: Repository<LocalFileStore, H>) -> Result<()> {
		let Some(head) = repo.refs().resolve_head().await? else {
			return Ok(()); // unborn branch — no commits
		};
		for oid in repo.rev_list(&[head]).await? {
			let (_, payload) = repo.objects().read_object(&oid).await?;
			let subject = parse_commit::<H>(&payload)?
				.message
				.lines()
				.next()
				.unwrap_or("")
				.to_owned();
			println!("{oid} {subject}");
		}
		Ok(())
	}
}
