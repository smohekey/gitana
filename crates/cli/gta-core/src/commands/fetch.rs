//! `gta fetch` — download new objects from the origin and update remote-tracking refs
//! (`refs/remotes/origin/*`), without touching the working tree.

use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{Deepen, Identity, TagFetch};
use gitana_remote::{
	self as transport, Connection, HttpPackFetcher, LocalConnection, LocalPackFetcher, PackFetcher,
	RemoteUrl, SshConnection, SshPackFetcher,
};

use crate::dispatch;
use crate::identity::CliIdentity;
use crate::shallow::build_fetch_deepen;
use crate::{
	CommandContext, RepositoryLayoutIdentity, git_config, repo, transport_for, url_rewrite,
};

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
	let identity = repo::capture_repository_layout_identity(&found)?;
	// The origin URL is `remote.origin.url` with `url.*.insteadOf` applied, read from the merged config.
	let (setup, common, git, _) = repo::command_setup_lease(&found, identity).await?;
	let config = git_config::for_worktree_at(common, git, &found.common_dir, &found.git_dir).await?;
	drop(setup);
	let url = url_rewrite::resolve_fetch_url(&config, "origin")?;
	let remote = RemoteUrl::parse(&url)?;
	if let Some(command) = CommandContext::current() {
		command.authorize(
			&config,
			&remote,
			gitana_remote::ProtocolContext::UserInitiated,
		)?;
	}
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
				identity,
				&body,
				FetchOptions {
					url: &display,
					tags,
					deepen: &deepen,
					unshallow,
				},
			)
			.await
		}
		RemoteUrl::Ssh(ssh) => {
			let ssh_cmd = crate::ssh::resolve_ssh_command(&config)?;
			let connection = SshConnection::open(&ssh, "git-upload-pack", &ssh_cmd, &askpass_cwd).await?;
			let body = connection.advertisement().to_vec();
			let mut fetcher = SshPackFetcher::new(connection);
			fetch_dispatch(
				&mut fetcher,
				&found,
				identity,
				&body,
				FetchOptions {
					url: &display,
					tags,
					deepen: &deepen,
					unshallow,
				},
			)
			.await
		}
		RemoteUrl::Local(path) => {
			let source = local_path(&askpass_cwd, &path);
			let source_layout = repo::inspect_root(&source).await?;
			let source_identity = repo::capture_repository_layout_identity(&source_layout)?;
			let (source_setup, common, git) =
				repo::revalidated_local_source_setup(&source_layout, source_identity, None).await?;
			match dispatch::detect_algorithm_at(&common, &source_layout.common_dir).await? {
				HashKind::Sha1 => {
					let second_common = common.try_clone()?;
					let second_git = git.try_clone()?;
					let source = repo::open_generic_from_dirs::<Sha1>(
						common,
						git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					let connection = LocalConnection::open(source).await?;
					let body = connection.advertisement().to_vec();
					let source = repo::open_generic_from_dirs::<Sha1>(
						second_common,
						second_git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					drop(source_setup);
					let mut fetcher = LocalPackFetcher::new(source);
					fetch_dispatch(
						&mut fetcher,
						&found,
						identity,
						&body,
						FetchOptions {
							url: &display,
							tags,
							deepen: &deepen,
							unshallow,
						},
					)
					.await
				}
				HashKind::Sha256 => {
					let second_common = common.try_clone()?;
					let second_git = git.try_clone()?;
					let source = repo::open_generic_from_dirs::<Sha256>(
						common,
						git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					let connection = LocalConnection::open(source).await?;
					let body = connection.advertisement().to_vec();
					let source = repo::open_generic_from_dirs::<Sha256>(
						second_common,
						second_git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					drop(source_setup);
					let mut fetcher = LocalPackFetcher::new(source);
					fetch_dispatch(
						&mut fetcher,
						&found,
						identity,
						&body,
						FetchOptions {
							url: &display,
							tags,
							deepen: &deepen,
							unshallow,
						},
					)
					.await
				}
			}
		}
	}
}

fn local_path(cwd: &Path, path: &str) -> std::path::PathBuf {
	let path = std::path::PathBuf::from(path);
	if path.is_absolute() {
		path
	} else {
		cwd.join(path)
	}
}

struct FetchOptions<'a> {
	url: &'a str,
	tags: TagFetch,
	deepen: &'a Deepen,
	unshallow: bool,
}

/// Negotiate the object format from the advertisement, then run the per-hash fetch over `fetcher`.
async fn fetch_dispatch(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	body: &[u8],
	options: FetchOptions<'_>,
) -> Result<()> {
	let (setup, common, _, _) = repo::command_setup_lease(found, identity).await?;
	let local = dispatch::detect_algorithm_at(&common, &found.common_dir).await?;
	drop(setup);
	transport::ensure_same_format(local, transport::negotiated_kind(body)?)?;
	match local {
		HashKind::Sha1 => fetch_into::<Sha1>(fetcher, found, identity, body, options).await,
		HashKind::Sha256 => fetch_into::<Sha256>(fetcher, found, identity, body, options).await,
	}
}

async fn fetch_into<H: HashAlgorithm>(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	body: &[u8],
	options: FetchOptions<'_>,
) -> Result<()> {
	let FetchOptions {
		url,
		tags,
		deepen,
		unshallow,
	} = options;
	let (setup, common, git, _) = repo::command_setup_lease(found, identity).await?;
	let repository =
		repo::open_generic_from_dirs::<H>(common, git, &found.git_dir, &found.common_dir).await?;
	// Preserve repository identity across the deliberately absent Windows publication window. The
	// porcelain receives this local-config snapshot so serialization need not cover the transfer.
	let bare = repo::repository_bare_snapshot(&repository).await?;
	drop(setup);
	// `--unshallow` only makes sense on a shallow repository; git rejects it on a complete one rather
	// than pointlessly refetch the whole history.
	if unshallow && repository.read_shallow().await?.is_empty() {
		bail!("--unshallow on a complete repository does not make sense");
	}
	// Every branch checked out in a worktree (this one and any linked one) so the porcelain can refuse a
	// refspec mapping onto it, naming the worktree's path as git does.
	let checkouts = repo::branch_checkouts(&found.common_dir, bare)
		.into_iter()
		.map(|(branch, path)| (branch, path.display().to_string()))
		.collect::<Vec<_>>();
	// git logs each advanced tracking ref as `<action>: <status>`; the committer falls back to a
	// placeholder when unconfigured, as git's reflog writes do. The action mirrors git: `GIT_REFLOG_ACTION`
	// if set, else `fetch` (a plain `gta fetch` names no remote, exactly like `git fetch`).
	let committer = CliIdentity::new(&repository).committer_or_default().await?;
	let action = crate::identity::reflog_action("fetch");
	let outcome = gitana_porcelain::fetch_with_bare(
		fetcher,
		&repository,
		bare,
		body,
		false,
		tags,
		false, // no --prune flag yet; a plain `gta fetch` matches git's default of not pruning
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
