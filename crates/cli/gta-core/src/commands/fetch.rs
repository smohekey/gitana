//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::Result;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_remote::{self as transport, Origin};

use crate::dispatch;
use crate::repo;

/// Fetch all branches from the origin into `refs/remotes/origin/*`.
pub async fn run(cwd: &Path) -> Result<()> {
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let body = transport::fetch_advertisement(&origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => fetch_into::<Sha1>(&origin, &found, &body).await,
		HashKind::Sha256 => fetch_into::<Sha256>(&origin, &found, &body).await,
	}
}

async fn fetch_into<H: HashAlgorithm>(
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir);
	gitana_porcelain::fetch(&repository, origin, body).await?;
	println!("Fetched from {}", origin.url);
	Ok(())
}
