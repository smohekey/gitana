//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_object::{HashKind, Sha1, Sha256};
use gitana_porcelain::Identity;
use gitana_remote::{self as transport, Origin};

use crate::identity::CliIdentity;
use crate::shallow::build_deepen;
use crate::{git_config, repo, transport_for};

/// Clone the repository at `url` into `dir` (default: the repo slug). Anonymous: works
/// for public repos. The local repository is created in whatever object format the
/// remote advertises.
///
/// `depth` / `shallow_since` / `shallow_exclude` request a shallow clone (git's `--depth`,
/// `--shallow-since`, `--shallow-exclude`): a truncated history recorded in `.git/shallow`.
pub async fn run(
	url: String,
	dir: Option<PathBuf>,
	depth: Option<u32>,
	shallow_since: Option<String>,
	shallow_exclude: Vec<String>,
) -> Result<()> {
	// Fail fast on a bad `--shallow-since` before any network round-trip.
	let deepen = build_deepen(depth, shallow_since.as_deref(), shallow_exclude)?;
	let origin = Origin::parse(&url)?;
	let target = dir.unwrap_or_else(|| PathBuf::from(origin.directory_name()));
	if target.exists()
		&& target
			.read_dir()
			.map(|mut entries| entries.next().is_some())
			.unwrap_or(false)
	{
		bail!(
			"destination path '{}' already exists and is not empty",
			target.display()
		);
	}

	// Negotiate the remote's object format before creating anything locally. Credentials resolve from
	// the ambient (global/system) config plus any URL userinfo — there is no local config yet — and the
	// one transport carries them through the advertisement GET and the pack POST alike.
	// A relative askpass resolves against the launch directory for `clone` (there is no worktree yet).
	let askpass_cwd = git_config::command_cwd().unwrap_or_else(|| PathBuf::from("."));
	let http = transport_for(git_config::ambient_effective().await?, &origin, askpass_cwd);
	let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
	let kind = transport::negotiated_kind(&body)?;

	// Create the git directory skeleton, like `init`.
	let git_dir = target.join(".git");
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}

	// A freshly cloned repository is an ordinary checkout: its per-worktree and common dirs coincide.
	// git records `clone: from <url>` on HEAD and the checked-out branch, using the URL **verbatim**
	// (trailing slash and all, before `Origin::parse` trims it) except with any `user:pass@` userinfo
	// stripped — git's `transport_anonymize_url` — so a credential in the URL is never persisted into
	// `.git/logs/*`. The committer falls back to a placeholder when unconfigured, as git's reflog writes
	// do. Resolved before `repo` moves into clone.
	let reflog_url = transport::anonymize_url(&url);
	match kind {
		HashKind::Sha1 => {
			let repo = repo::open_generic::<Sha1>(&git_dir, &git_dir).await?;
			let committer = CliIdentity::new(&repo).committer_or_default().await?;
			gitana_porcelain::clone(
				&http,
				repo,
				&origin,
				&body,
				repo::open_work_dir(&target)?,
				&deepen,
				Some(gitana_porcelain::CloneReflog {
					committer: &committer,
					url: &reflog_url,
				}),
			)
			.await?;
		}
		HashKind::Sha256 => {
			let repo = repo::open_generic::<Sha256>(&git_dir, &git_dir).await?;
			let committer = CliIdentity::new(&repo).committer_or_default().await?;
			gitana_porcelain::clone(
				&http,
				repo,
				&origin,
				&body,
				repo::open_work_dir(&target)?,
				&deepen,
				Some(gitana_porcelain::CloneReflog {
					committer: &committer,
					url: &reflog_url,
				}),
			)
			.await?;
		}
	}

	println!("Cloned '{}' into '{}'", origin.url, target.display());
	Ok(())
}
