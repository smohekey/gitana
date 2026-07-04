//! `gta pull` — fetch from the origin (updating remote-tracking refs) and integrate the current
//! branch's upstream: fast-forward, or a true merge commit when the histories have diverged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_remote::{self as transport, Origin};
use gitana_repository::HeadState;
use gitana_worktree::WorkTree;

use crate::commands::merge;
use crate::dispatch;
use crate::identity::CliIdentity;
use crate::repo;

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let body = transport::fetch_advertisement(&origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => pull_into::<Sha1>(&origin, &found, &body).await,
		HashKind::Sha256 => pull_into::<Sha256>(&origin, &found, &body).await,
	}
}

/// Fetch into the remote-tracking refs, then merge the current branch's upstream tip. Both are the
/// porcelain composites; this composes them, printing the "Fetched from" line *between* — so a merge
/// that then fails (e.g. a dirty work tree) still reports the completed fetch, as git does.
async fn pull_into<H: HashAlgorithm>(
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
) -> Result<()> {
	let work = found
		.work
		.clone()
		.context("cannot pull in a bare repository")?;
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir)?;
	let worktree = WorkTree::new(repository, work, found.git_dir.clone());

	// `update_head_ok`: a fetch refspec may map straight into the checked-out branch (a mirror config
	// like `+refs/heads/*:refs/heads/*`); the merge below advances that branch and the work tree.
	let outcome = gitana_porcelain::fetch(worktree.repository(), origin, body, true).await?;
	println!("Fetched from {}", origin.url);
	// A rejected (non-fast-forward) tracking update is a failed fetch; do not merge a stale upstream.
	if !outcome.rejected.is_empty() {
		bail!("some remote-tracking refs were not updated (non-fast-forward)");
	}

	// The upstream tip is the current branch's remote branch, read straight from the advertisement —
	// the merge source, whatever tracking ref (if any) the fetch refspecs routed it to.
	let branch = match worktree.repository().refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot pull onto a detached HEAD"),
	};
	let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
	let upstream = gitana_porcelain::pull_upstream(worktree.repository(), body, &branch)
		.await?
		.with_context(|| format!("origin has no {short} to merge (or a refspec excludes it)"))?;
	let message = format!("Merge branch '{short}' of {}", origin.url);

	let identity = CliIdentity::new(worktree.repository());
	let outcome = gitana_porcelain::merge(
		&worktree,
		&upstream.to_hex(),
		Some(message),
		false,
		false,
		&identity,
	)
	.await?;
	merge::render(outcome)
}
