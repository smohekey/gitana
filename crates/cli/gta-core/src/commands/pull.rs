//! `gta pull` — fetch from the origin (updating remote-tracking refs) and integrate the current
//! branch's upstream: fast-forward, or a true merge commit when the histories have diverged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::Identity;
use gitana_remote::{self as transport, HttpTransport, Origin};
use gitana_repository::HeadState;
use gitana_worktree::WorkTree;

use crate::commands::merge;
use crate::dispatch;
use crate::identity::CliIdentity;
use crate::signer;
use crate::{git_config, repo, transport_for, url_rewrite};

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	let found = repo::discover(cwd).await?;
	// The origin URL is `remote.origin.url` with `url.*.insteadOf` applied, read from the merged config.
	let config = git_config::effective_config_at(&found.git_dir, &found.common_dir).await?;
	let origin = url_rewrite::fetch_origin(&config, "origin")?;
	// A relative askpass resolves against the worktree root, as git runs it from there (bare: git dir).
	let askpass_cwd = found
		.worktree_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());
	let http = transport_for(config, &origin, askpass_cwd)?;
	let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => pull_into::<Sha1>(&http, &origin, &found, &body, cwd).await,
		HashKind::Sha256 => pull_into::<Sha256>(&http, &origin, &found, &body, cwd).await,
	}
}

/// Fetch into the remote-tracking refs, then merge the current branch's upstream tip. Both are the
/// porcelain composites; this composes them, printing the "Fetched from" line *between* — so a merge
/// that then fails (e.g. a dirty work tree) still reports the completed fetch, as git does.
async fn pull_into<H: HashAlgorithm>(
	http: &impl HttpTransport,
	origin: &Origin,
	found: &repo::RepositoryLayout,
	body: &[u8],
	cwd: &Path,
) -> Result<()> {
	let work = found
		.worktree_root
		.clone()
		.context("cannot pull in a bare repository")?;
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir).await?;
	let worktree = WorkTree::new(
		repository,
		repo::open_work_dir(&work)?,
		found.git_dir.clone(),
	);

	// Every branch checked out in a worktree. `update_head_ok` below exempts only *this* worktree's
	// branch (advanced by the merge); a branch checked out in another worktree is still refused, since
	// pull's merge advances only this worktree's HEAD.
	let checkouts = repo::branch_checkouts(&found.common_dir)
		.into_iter()
		.map(|(branch, path)| (branch, path.display().to_string()))
		.collect::<Vec<_>>();
	// `update_head_ok`: a fetch refspec may map straight into the checked-out branch (a mirror config
	// like `+refs/heads/*:refs/heads/*`); the merge below advances that branch and the work tree.
	let identity = CliIdentity::new(worktree.repository());
	// Under a pull, git reflogs the tracking-ref updates with the `pull` action (not `fetch`), honouring
	// `GIT_REFLOG_ACTION` if set. The merge step below records HEAD/branch separately; this covers only
	// the tracking refs.
	let committer = identity.committer_or_default().await?;
	let action = crate::identity::reflog_action("pull");
	let outcome = gitana_porcelain::fetch(
		http,
		worktree.repository(),
		origin,
		body,
		true,
		gitana_porcelain::TagFetch::Auto,
		&gitana_porcelain::Deepen::default(),
		&checkouts,
		Some(gitana_porcelain::FetchReflog {
			committer: &committer,
			action: &action,
		}),
	)
	.await?;
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

	// A pull's merge commit is signed when git config requests it, like a plain `gta merge`.
	let signer = signer::config_signer(worktree.repository(), cwd).await?;
	let outcome = gitana_porcelain::merge(
		&worktree,
		&upstream.to_hex(),
		Some(message),
		false,
		false,
		&identity,
		signer.as_ref(),
	)
	.await?;
	merge::render(outcome)
}
