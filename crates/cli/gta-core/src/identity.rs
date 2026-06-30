//! Git identity (author/committer) resolution, shared by the commands that write objects or
//! reflogs. A git identity line is `Name <email> seconds ±hhmm`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::{Context, Result};
use gitana_object::HashAlgorithm;
use gitana_porcelain::Identity;
use gitana_repository::Repository;

/// The CLI's [`Identity`] for porcelain operations: resolves author/committer from `GIT_*` env and
/// git config via [`signature`] / [`signature_or_default`]. Holds the repository so config lookups
/// happen lazily, only when an operation actually asks for a signature.
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
	async fn committer_or_default(&self) -> String {
		signature_or_default(self.repo, "COMMITTER").await
	}
}

/// Build a `role` (`AUTHOR` or `COMMITTER`) identity line from the `GIT_<role>_*` environment,
/// falling back to `user.name`/`user.email` in config. Errors when neither is set — for
/// operations like `commit` that must record a real identity.
pub async fn signature<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	role: &str,
) -> Result<String> {
	let (name, email) = configured(repo, role).await;
	let name =
		name.with_context(|| format!("identity name not set (GIT_{role}_NAME or user.name)"))?;
	let email =
		email.with_context(|| format!("identity email not set (GIT_{role}_EMAIL or user.email)"))?;
	Ok(format!("{name} <{email}> {}", date(role)))
}

/// Like [`signature`], but never fails: an unset name or email falls back to a placeholder.
/// Used for reflog entries (e.g. `reset`), which git records without requiring configuration.
pub async fn signature_or_default<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	role: &str,
) -> String {
	let (name, email) = configured(repo, role).await;
	let name = name.unwrap_or_else(|| "unknown".to_owned());
	let email = email.unwrap_or_else(|| "unknown@localhost".to_owned());
	format!("{name} <{email}> {}", date(role))
}

/// The configured `(name, email)` for `role` from the environment, then git config.
async fn configured<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	role: &str,
) -> (Option<String>, Option<String>) {
	let config = repo.read_config().await.ok();
	let from_config = |key: &str| {
		config
			.as_ref()
			.and_then(|c| c.get_string("user", None, key).map(str::to_owned))
	};
	let name = std::env::var(format!("GIT_{role}_NAME"))
		.ok()
		.or_else(|| from_config("name"));
	let email = std::env::var(format!("GIT_{role}_EMAIL"))
		.ok()
		.or_else(|| from_config("email"));
	(name, email)
}

/// The `GIT_<role>_DATE` override, or the current time as `seconds +0000`.
fn date(role: &str) -> String {
	std::env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| {
		let secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);
		format!("{secs} +0000")
	})
}
