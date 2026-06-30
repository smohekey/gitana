//! `gta pull` — fetch from the origin (updating remote-tracking refs) and integrate the current
//! branch's upstream: fast-forward, or a true merge commit when the histories have diverged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_repository::HeadState;

use crate::commands::{fetch, merge};
use crate::repo;
use crate::transport::Origin;

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	let (_, git_dir) = repo::discover(cwd)?;
	let repository = repo::open(&git_dir);

	let branch = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot pull onto a detached HEAD"),
	};

	// Fetch all branches and update the remote-tracking refs (`refs/remotes/origin/*`), like git —
	// so the fetched tip is recorded under a ref even if the integration below fails.
	let advertised = fetch::run(cwd).await?;

	// Take the branch's tip from the *current* advertisement, so an upstream branch that no longer
	// exists is reported rather than integrating a possibly-stale remote-tracking ref.
	let remote_tip = advertised
		.oid_of(&branch)
		.with_context(|| format!("origin has no {branch}"))?;

	let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
	let message = format!("Merge branch '{short}' of {}", Origin::load(&git_dir)?.url);
	merge::run(
		cwd,
		Some(remote_tip.to_hex()),
		Some(message),
		false,
		false,
		false,
		false,
	)
	.await
}
