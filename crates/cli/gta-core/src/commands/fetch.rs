//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::Result;
use gitana_git_http::parse_advertisement;
use gitana_object::{HashAlgorithm, Sha1, Sha256};

use crate::dispatch::{self, HashKind};
use crate::repo;
use crate::transport::{self, Origin};

/// Fetch all branches from the origin into `refs/remotes/origin/*`.
pub async fn run(cwd: &Path) -> Result<()> {
	let (_work, git_dir) = repo::discover(cwd)?;
	let origin = Origin::load(&git_dir)?;
	let body = transport::fetch_advertisement(&origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&git_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => fetch_into::<Sha1>(&origin, &git_dir, &body).await,
		HashKind::Sha256 => fetch_into::<Sha256>(&origin, &git_dir, &body).await,
	}
}

async fn fetch_into<H: HashAlgorithm>(origin: &Origin, git_dir: &Path, body: &[u8]) -> Result<()> {
	let repository = repo::open_generic::<H>(git_dir);
	let advertised = parse_advertisement::<H>(body)?;
	let wants = transport::advertised_oids(&advertised);
	let haves = transport::local_haves(&repository).await?;
	transport::fetch_pack(origin, &repository, &wants, &haves).await?;

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
