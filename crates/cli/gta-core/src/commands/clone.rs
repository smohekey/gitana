//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{Deepen, Identity};
use gitana_remote::{
	self as transport, Connection, HttpConnection, Origin, RemoteUrl, SshConnection,
};

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
	let config = git_config::from_ambient().await?;
	let remote = RemoteUrl::parse(&url_rewrite::rewrite_fetch_url(&config, &url)?)?;
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

	// git records `clone: from <url>` on HEAD and the checked-out branch, using the URL **verbatim**
	// (trailing slash and all, before parsing trims it) except with any `user:pass@` userinfo stripped —
	// git's `transport_anonymize_url` — so a credential in the URL is never persisted into `.git/logs/*`.
	let reflog_url = transport::anonymize_url(&url);
	// Record the ORIGINAL clone argument in `remote.origin.url` (not the `insteadOf`-rewritten transport
	// URL), so a later change to the rewrite rules still applies on subsequent fetches. The *original
	// spelling* is preserved — including a trailing slash the rewrite prefix may depend on and an SSH /
	// scp-like alias. gitana additionally redacts any password (git persists it verbatim; gitana
	// deliberately never writes a plaintext credential to `.git/config`, as on a plain userinfo clone).
	let persist_url = transport::redact_password(&url);
	let git_dir = target.join(".git");
	// The directory external helpers run from — git's effective working directory (`gta`'s `-C`, or the
	// launch dir): where a relative askpass (HTTP) or a relative `GIT_SSH_COMMAND` / key path (SSH)
	// resolves. There is no worktree yet, so it is the launch/`-C` directory, matching git.
	let command_cwd = git_config::command_cwd().unwrap_or_else(|| PathBuf::from("."));

	// Open the transport as a connection, negotiate the object format from the advertisement, then run
	// the porcelain clone over it — one code path for both HTTP and SSH.
	match remote {
		RemoteUrl::Http(origin) => {
			// Credentials resolve from the ambient (global/system) config plus any URL userinfo — there is
			// no local config yet — and the one transport carries them through the advertisement GET and the
			// pack POST alike.
			let http = transport_for(config, &origin, command_cwd)?;
			let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
			let kind = transport::negotiated_kind(&body)?;
			create_skeleton(&git_dir)?;
			let mut connection = HttpConnection::new(
				&http,
				origin.upload_pack(),
				transport::UPLOAD_PACK_REQUEST,
				body,
			);
			clone_over(
				&mut connection,
				kind,
				&git_dir,
				&target,
				&deepen,
				&reflog_url,
				&persist_url,
			)
			.await?;
		}
		RemoteUrl::Ssh(ssh) => {
			// SSH sends the ref advertisement on connect — no separate GET — so opening the connection
			// yields it directly. gitana drives the user's `ssh` (resolved from git's `GIT_SSH_COMMAND` /
			// `core.sshCommand` / `GIT_SSH` precedence and variant), run from the effective command
			// directory so a relative command / key resolves as git's would.
			let ssh_cmd = crate::ssh::resolve_ssh_command(&config)?;
			let mut connection =
				SshConnection::open(&ssh, "git-upload-pack", &ssh_cmd, &command_cwd).await?;
			let kind = transport::negotiated_kind(connection.advertisement())?;
			create_skeleton(&git_dir)?;
			clone_over(
				&mut connection,
				kind,
				&git_dir,
				&target,
				&deepen,
				&reflog_url,
				&persist_url,
			)
			.await?;
		}
	}

	// Report the userinfo-stripped URL — a password in the clone URL must not reach stdout / CI logs.
	println!("Cloned '{}' into '{}'", reflog_url, target.display());
	Ok(())
}

/// Create the git directory skeleton, like `init`.
fn create_skeleton(git_dir: &Path) -> Result<()> {
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}
	Ok(())
}

/// Run the porcelain clone over `connection`, dispatching the repository's hash algorithm (negotiated
/// from the advertisement) so the rest is generic over `H`.
async fn clone_over(
	connection: &mut impl Connection,
	kind: HashKind,
	git_dir: &Path,
	target: &Path,
	deepen: &Deepen,
	reflog_url: &str,
	persist_url: &str,
) -> Result<()> {
	match kind {
		HashKind::Sha1 => {
			clone_as::<Sha1>(connection, git_dir, target, deepen, reflog_url, persist_url).await
		}
		HashKind::Sha256 => {
			clone_as::<Sha256>(connection, git_dir, target, deepen, reflog_url, persist_url).await
		}
	}
}

async fn clone_as<H: HashAlgorithm>(
	connection: &mut impl Connection,
	git_dir: &Path,
	target: &Path,
	deepen: &Deepen,
	reflog_url: &str,
	persist_url: &str,
) -> Result<()> {
	let repo = repo::open_generic::<H>(git_dir, git_dir).await?;
	// The committer falls back to a placeholder when unconfigured, as git's reflog writes do.
	let committer = CliIdentity::new(&repo).committer_or_default().await?;
	gitana_porcelain::clone(
		connection,
		repo,
		repo::open_work_dir(target)?,
		deepen,
		Some(gitana_porcelain::CloneReflog {
			committer: &committer,
			url: reflog_url,
		}),
		persist_url,
	)
	.await
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_dir_from_original_url() {
		assert_eq!(default_directory_name("https://alias/input.git"), "input");
		assert_eq!(default_directory_name("https://host/a/b/"), "b");
		// A non-http (scp-like) alias still yields a sensible slug.
		assert_eq!(default_directory_name("git@host:org/repo.git"), "repo");
	}
}
