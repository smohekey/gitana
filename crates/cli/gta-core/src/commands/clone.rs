//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_object::{HashKind, Sha1, Sha256};
use gitana_porcelain::Identity;
use gitana_remote::{self as transport, Origin};

use crate::identity::CliIdentity;
use crate::shallow::build_deepen;
use crate::{git_config, repo, transport_for, url_rewrite};

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
	// Apply `url.*.insteadOf` before parsing, from the ambient (global/system) config — there is no local
	// config yet. git rewrites the transport URL this way (so a `git@…`-style alias could even map to
	// https). The default checkout directory comes from the *original* argument (git's `guess_dir_name`),
	// not the rewritten URL, in case a rewrite changes the last path segment.
	let config = git_config::ambient_effective().await?;
	let origin = Origin::parse(&url_rewrite::rewrite_fetch_url(&config, &url)?)?;
	let target = dir.unwrap_or_else(|| PathBuf::from(default_directory_name(&url)));
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
	let http = transport_for(config, &origin, askpass_cwd)?;
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
	// Record the ORIGINAL clone argument in `remote.origin.url` (not the `insteadOf`-rewritten transport
	// URL), so a later change to the rewrite rules still applies on subsequent fetches. The *original
	// spelling* is preserved — including a trailing slash the rewrite prefix may depend on and a non-http
	// (`scp`-like) alias `Origin::parse` would reject. gitana additionally redacts any password (git
	// persists it verbatim; gitana deliberately never writes a plaintext credential to `.git/config`, as
	// on a plain userinfo clone). The one cost of that safety choice is a password embedded *in an
	// `insteadOf` prefix* (a very unusual config) — the redacted url no longer matches that prefix on a
	// later fetch; the username-bearing prefix still does.
	let persist_url = redact_url_password(&url);
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
				Some(&persist_url),
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
				Some(&persist_url),
			)
			.await?;
		}
	}

	println!("Cloned '{}' into '{}'", origin.url, target.display());
	Ok(())
}

/// The default checkout directory name for the *original* clone argument (git's `guess_dir_name`): an
/// http(s) URL reuses [`Origin::directory_name`]; any other spelling (e.g. an `scp`-like `git@host:org/
/// repo.git` alias) takes the last `/`- or `:`-delimited segment, minus a `.git` suffix.
fn default_directory_name(url: &str) -> String {
	if let Ok(origin) = Origin::parse(url) {
		return origin.directory_name();
	}
	let last = url
		.trim_end_matches('/')
		.rsplit(['/', ':'])
		.find(|segment| !segment.is_empty())
		.unwrap_or("repository");
	let name = last.strip_suffix(".git").unwrap_or(last);
	if name.is_empty() {
		"repository".to_owned()
	} else {
		name.to_owned()
	}
}

/// The clone URL to persist in `remote.origin.url`: the original `url` verbatim (scheme case, trailing
/// slash, path all preserved, so it still matches the `insteadOf` rule on a later fetch) with only a
/// `:password` stripped from the userinfo. git persists the password too; gitana deliberately does not,
/// keeping a plaintext credential out of `.git/config` (its established behaviour for a userinfo clone).
/// A string with no `://` (an `scp`-like alias, which carries no password) is returned unchanged.
fn redact_url_password(url: &str) -> String {
	let Some((scheme, rest)) = url.split_once("://") else {
		return url.to_owned();
	};
	let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
	let (authority, tail) = rest.split_at(authority_end);
	// The userinfo is delimited by the last `@`; the username is up to the first `:` (so a password may
	// contain either). An empty username drops the whole `userinfo@`.
	let authority = match authority.rsplit_once('@') {
		Some((userinfo, host)) => {
			let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
			if user.is_empty() {
				host.to_owned()
			} else {
				format!("{user}@{host}")
			}
		}
		None => authority.to_owned(),
	};
	format!("{scheme}://{authority}{tail}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redacts_password_preserving_spelling() {
		// Password dropped, username + trailing slash + scheme case preserved.
		assert_eq!(
			redact_url_password("HTTPS://alice:secret@host/repo/"),
			"HTTPS://alice@host/repo/"
		);
		// A password containing `@`/`:` is still fully removed (last `@`, first `:`).
		assert_eq!(
			redact_url_password("https://alice:se@cr:et@host/r"),
			"https://alice@host/r"
		);
		// No userinfo, and a userinfo-less scp-like alias, pass through unchanged.
		assert_eq!(redact_url_password("https://host/r"), "https://host/r");
		assert_eq!(
			redact_url_password("git@host:org/repo.git"),
			"git@host:org/repo.git"
		);
		// An empty username drops the whole userinfo.
		assert_eq!(
			redact_url_password("https://:secret@host/r"),
			"https://host/r"
		);
	}

	#[test]
	fn default_dir_from_original_url() {
		assert_eq!(default_directory_name("https://alias/input.git"), "input");
		assert_eq!(default_directory_name("https://host/a/b/"), "b");
		// A non-http (scp-like) alias still yields a sensible slug.
		assert_eq!(default_directory_name("git@host:org/repo.git"), "repo");
	}
}
