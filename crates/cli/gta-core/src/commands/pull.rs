//! `gta pull` — fetch from the origin (updating remote-tracking refs) and integrate the current
//! branch's upstream: fast-forward, or a true merge commit when the histories have diverged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_repository::HeadState;

use crate::commands::{fetch, merge};
use crate::dispatch::{self, HashKind};
use crate::repo;
use crate::transport::Origin;

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	// Fetch into the remote-tracking refs, then integrate the upstream tip via `merge`
	// (fast-forward, or a true merge commit when the histories have diverged).
	fetch::run(cwd).await?;

	let (_work, git_dir) = repo::discover(cwd)?;
	let kind = dispatch::detect_algorithm(&git_dir)?;
	let (branch, remote_tip) = match kind {
		HashKind::Sha1 => upstream_tip::<Sha1>(&git_dir).await?,
		HashKind::Sha256 => upstream_tip::<Sha256>(&git_dir).await?,
	};

	let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
	let message = format!("Merge branch '{short}' of {}", Origin::load(&git_dir)?.url);
	merge::run(
		cwd,
		Some(remote_tip),
		Some(message),
		false,
		false,
		false,
		false,
	)
	.await
}

/// The current branch and its upstream tip (`refs/remotes/origin/<branch>`, updated by the
/// preceding fetch) as a hex id. Errors on a detached HEAD or a missing upstream.
async fn upstream_tip<H: HashAlgorithm>(git_dir: &Path) -> Result<(String, String)> {
	let repository = repo::open_generic::<H>(git_dir);
	let branch = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot pull onto a detached HEAD"),
	};
	let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
	let tracking = format!("refs/remotes/origin/{short}");
	let tip = repository
		.refs()
		.resolve(&tracking)
		.await?
		.with_context(|| format!("origin has no {short}"))?;
	Ok((branch, tip.to_hex()))
}
