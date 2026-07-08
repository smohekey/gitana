//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_object::{HashKind, Sha1, Sha256};
use gitana_remote::{self as transport, Origin, ReqwestTransport};

use crate::repo;
use crate::shallow::build_deepen;

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

	// Negotiate the remote's object format before creating anything locally.
	let http = ReqwestTransport::new();
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
	match kind {
		HashKind::Sha1 => {
			let repo = repo::open_generic::<Sha1>(&git_dir, &git_dir)?;
			gitana_porcelain::clone(
				&http,
				repo,
				&origin,
				&body,
				repo::open_work_dir(&target)?,
				&deepen,
			)
			.await?;
		}
		HashKind::Sha256 => {
			let repo = repo::open_generic::<Sha256>(&git_dir, &git_dir)?;
			gitana_porcelain::clone(
				&http,
				repo,
				&origin,
				&body,
				repo::open_work_dir(&target)?,
				&deepen,
			)
			.await?;
		}
	}

	println!("Cloned '{}' into '{}'", origin.url, target.display());
	Ok(())
}
