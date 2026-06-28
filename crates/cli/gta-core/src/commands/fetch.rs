//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::Result;

use crate::repo;
use crate::transport::{self, Origin, advertised_oids, local_haves};

/// Fetch all branches from the origin into `refs/remotes/origin/*`.
pub async fn run(cwd: &Path) -> Result<()> {
	let (_work, git_dir) = repo::discover(cwd)?;
	let repository = repo::open(&git_dir);
	let origin = Origin::load(&git_dir)?;

	let advertised = transport::discover_upload(&origin).await?;
	let wants = advertised_oids(&advertised);
	let haves = local_haves(&repository).await?;
	transport::fetch_pack(&origin, &repository, &wants, &haves).await?;

	for (name, oid) in advertised.branches() {
		let short = name.strip_prefix("refs/heads/").unwrap_or(name);
		let tracking = format!("refs/remotes/origin/{short}");
		let current = repository.refs().resolve(&tracking).await?;
		if current != Some(oid) {
			repository
				.refs()
				.update_ref(&tracking, oid, current)
				.await?;
		}
	}

	println!("Fetched from {}", origin.url);
	Ok(())
}
