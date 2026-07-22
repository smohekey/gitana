//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{Deepen, Identity, TagFetch};
use gitana_remote::{
	self as transport, Connection, HttpPackFetcher, PackFetcher, RemoteUrl, SshConnection,
	SshPackFetcher,
};

use crate::dispatch;
use crate::identity::CliIdentity;
use crate::shallow::build_fetch_deepen;
use crate::{git_config, repo, transport_for, url_rewrite};

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
	let found = repo::discover(cwd).await?;
	// The origin URL is `remote.origin.url` with `url.*.insteadOf` applied, read from the merged config.
	let config = git_config::effective_config_at(&found.git_dir, &found.common_dir).await?;
	let url = url_rewrite::resolve_fetch_url(&config, "origin")?;
	let remote = RemoteUrl::parse(&url)?;
	// A credential-free form for the "Fetched from" line — *all* userinfo stripped (a token can occupy
	// the username field), since the raw `url` is only for the auth-bearing transport parse above.
	let display = transport::anonymize_url(&url);
	// A relative askpass (HTTP) / `GIT_SSH_COMMAND` (SSH) resolves against the worktree root, as git runs
	// it from there (bare: git dir).
	let askpass_cwd = found
		.worktree_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());
	let tags = match (all_tags, no_tags) {
		(true, _) => TagFetch::All,
		(_, true) => TagFetch::None,
		_ => TagFetch::Auto,
	};

	// Open the transport as a pack fetcher (HTTP stateless-RPC, or the SSH stateful stream), then run the
	// dispatch — one path for both, differing only in how the negotiation downloads the pack.
	match remote {
		RemoteUrl::Http(origin) => {
			let http = transport_for(config, &origin, askpass_cwd)?;
			let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
			let mut fetcher = HttpPackFetcher::new(&http, &origin);
			fetch_dispatch(
				&mut fetcher,
				&found,
				&body,
				&display,
				tags,
				&deepen,
				unshallow,
			)
			.await
		}
		RemoteUrl::Ssh(ssh) => {
			let connection = SshConnection::open(&ssh, "git-upload-pack", &askpass_cwd).await?;
			let body = connection.advertisement().to_vec();
			let mut fetcher = SshPackFetcher::new(connection);
			fetch_dispatch(
				&mut fetcher,
				&found,
				&body,
				&display,
				tags,
				&deepen,
				unshallow,
			)
			.await
		}
	}
}

/// Negotiate the object format from the advertisement, then run the per-hash fetch over `fetcher`.
async fn fetch_dispatch(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	body: &[u8],
	url: &str,
	tags: TagFetch,
	deepen: &Deepen,
	unshallow: bool,
) -> Result<()> {
	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(body)?)?;
	match local {
		HashKind::Sha1 => fetch_into::<Sha1>(fetcher, found, body, url, tags, deepen, unshallow).await,
		HashKind::Sha256 => {
			fetch_into::<Sha256>(fetcher, found, body, url, tags, deepen, unshallow).await
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn fetch_into<H: HashAlgorithm>(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	body: &[u8],
	url: &str,
	tags: TagFetch,
	deepen: &Deepen,
	unshallow: bool,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir).await?;
	// `--unshallow` only makes sense on a shallow repository; git rejects it on a complete one rather
	// than pointlessly refetch the whole history.
	if unshallow && repository.read_shallow().await?.is_empty() {
		bail!("--unshallow on a complete repository does not make sense");
	}
	// Every branch checked out in a worktree (this one and any linked one) so the porcelain can refuse a
	// refspec mapping onto it, naming the worktree's path as git does.
	let checkouts = repo::branch_checkouts(&found.common_dir)
		.into_iter()
		.map(|(branch, path)| (branch, path.display().to_string()))
		.collect::<Vec<_>>();
	// git logs each advanced tracking ref as `<action>: <status>`; the committer falls back to a
	// placeholder when unconfigured, as git's reflog writes do. The action mirrors git: `GIT_REFLOG_ACTION`
	// if set, else `fetch` (a plain `gta fetch` names no remote, exactly like `git fetch`).
	let committer = CliIdentity::new(&repository).committer_or_default().await?;
	let action = crate::identity::reflog_action("fetch");
	let outcome = gitana_porcelain::fetch(
		fetcher,
		&repository,
		body,
		false,
		tags,
		deepen,
		&checkouts,
		Some(gitana_porcelain::FetchReflog {
			committer: &committer,
			action: &action,
		}),
	)
	.await?;
	println!("Fetched from {url}");
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
