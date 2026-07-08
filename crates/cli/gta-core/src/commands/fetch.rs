//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{Deepen, TagFetch};
use gitana_remote::{self as transport, Origin, ReqwestTransport};

use crate::dispatch;
use crate::repo;
use crate::shallow::build_fetch_deepen;

/// Fetch all branches from the origin into `refs/remotes/origin/*`. By default git's tag auto-follow
/// also lands tags reachable from the fetched branches; `all_tags` (`--tags`) mirrors every advertised
/// `refs/tags/*`, and `no_tags` (`--no-tags`) disables tag fetching entirely. The two are exclusive.
///
/// The shallow flags mirror git's: `depth` / `shallow_since` / `shallow_exclude` bound the fetched
/// history like `clone` does, `deepen` extends the current shallow boundary by a relative number of
/// commits, and `unshallow` fills in the complete history. They are mutually exclusive per
/// [`build_fetch_deepen`].
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	all_tags: bool,
	no_tags: bool,
	depth: Option<u32>,
	deepen: Option<u32>,
	unshallow: bool,
	shallow_since: Option<String>,
	shallow_exclude: Vec<String>,
) -> Result<()> {
	// Validate the shallow flags before any network round-trip.
	let deepen = build_fetch_deepen(
		depth,
		deepen,
		unshallow,
		shallow_since.as_deref(),
		shallow_exclude,
	)?;
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let http = ReqwestTransport::new();
	let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	let tags = match (all_tags, no_tags) {
		(true, _) => TagFetch::All,
		(_, true) => TagFetch::None,
		_ => TagFetch::Auto,
	};
	match local {
		HashKind::Sha1 => {
			fetch_into::<Sha1>(&http, &origin, &found, &body, tags, &deepen, unshallow).await
		}
		HashKind::Sha256 => {
			fetch_into::<Sha256>(&http, &origin, &found, &body, tags, &deepen, unshallow).await
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn fetch_into<H: HashAlgorithm>(
	http: &ReqwestTransport,
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
	tags: TagFetch,
	deepen: &Deepen,
	unshallow: bool,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir)?;
	// `--unshallow` only makes sense on a shallow repository; git rejects it on a complete one rather
	// than pointlessly refetch the whole history.
	if unshallow && repository.read_shallow().await?.is_empty() {
		bail!("--unshallow on a complete repository does not make sense");
	}
	let outcome =
		gitana_porcelain::fetch(http, &repository, origin, body, false, tags, deepen).await?;
	println!("Fetched from {}", origin.url);
	for (tracking, _) in &outcome.updated {
		println!("   {tracking}");
	}
	for tracking in &outcome.rejected {
		eprintln!(" ! {tracking} (non-fast-forward, not updated)");
	}
	// git exits non-zero when a ref update was rejected, even though the rest were applied.
	if !outcome.rejected.is_empty() {
		bail!("some remote-tracking refs were not updated (non-fast-forward)");
	}
	Ok(())
}
