//! The CLI side of git identity resolution: read the `GIT_<role>_*` process environment and the
//! clock, and hand them to [`gitana_identity`], which composes the override over git config and
//! formats the `Name <email> seconds ±hhmm` line. Reading process env and the clock is a frontend
//! concern; the reusable resolution + formatting lives in the core crate.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_porcelain::Identity;
use gitana_repository::Repository;

/// The CLI's [`Identity`] for porcelain operations. Holds the repository so config lookups happen
/// lazily, only when an operation actually asks for a signature.
pub(crate) struct CliIdentity<'a, H: HashAlgorithm> {
	repo: &'a Repository<Backend, H>,
}

impl<'a, H: HashAlgorithm> CliIdentity<'a, H> {
	pub(crate) fn new(repo: &'a Repository<Backend, H>) -> Self {
		Self { repo }
	}
}

impl<H: HashAlgorithm> Identity for CliIdentity<'_, H> {
	async fn author(&self) -> Result<String> {
		signature(self.repo, "AUTHOR").await
	}
	async fn committer(&self) -> Result<String> {
		signature(self.repo, "COMMITTER").await
	}
	async fn committer_or_default(&self) -> Result<String> {
		signature_or_default(self.repo, "COMMITTER").await
	}
}

/// Build a `role` (`AUTHOR` or `COMMITTER`) identity line from the `GIT_<role>_*` environment,
/// falling back to `user.name`/`user.email` in config. Errors when neither is set — for operations
/// like `commit` that must record a real identity. Config is resolved across git's full precedence
/// stack (system, global, local), so a globally-configured identity is honoured.
pub async fn signature<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	role: &str,
) -> Result<String> {
	let config = crate::git_config::effective_config(repo).await?;
	gitana_identity::signature(
		role,
		env_override(role, "NAME"),
		env_override(role, "EMAIL"),
		Some(&config),
		&when(role),
	)
}

/// Like [`signature`], but defaults a *missing* name or email to a placeholder rather than failing —
/// for reflog entries (e.g. `reset`) that git records without a configured identity. It still errors
/// if the config stack itself cannot be read (a malformed global/system file), as git aborts the
/// whole operation on a bad config. Config is resolved across git's full precedence stack.
pub async fn signature_or_default<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	role: &str,
) -> Result<String> {
	let config = crate::git_config::effective_config(repo).await?;
	Ok(gitana_identity::signature_or_default(
		env_override(role, "NAME"),
		env_override(role, "EMAIL"),
		Some(&config),
		&when(role),
	))
}

/// The `GIT_<role>_<field>` environment override, if set.
fn env_override(role: &str, field: &str) -> Option<String> {
	std::env::var(format!("GIT_{role}_{field}")).ok()
}

/// The reflog action prefix for a remote op, mirroring git: the `GIT_REFLOG_ACTION` override whenever
/// the variable is *set* (git treats even an explicit empty value as set, recording `: <status>`), else
/// `default` — the command name git would use for that invocation (`fetch` for a plain `gta fetch`,
/// which like `git fetch` names no remote; `pull` for `gta pull`).
pub(crate) fn reflog_action(default: &str) -> String {
	std::env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| default.to_owned())
}

/// The commit time: the `GIT_<role>_DATE` override, or the current time as `seconds +0000`.
fn when(role: &str) -> String {
	std::env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| {
		let secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);
		format!("{secs} +0000")
	})
}
