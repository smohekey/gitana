use std::path::Path;

use anyhow::{Result, bail};

use crate::identity;
use crate::repo;

/// `reset` in its two forms.
///
/// Without `paths`: move the current branch (or detached `HEAD`) to `target` (default `HEAD`),
/// then — by mode — reset the index (`--mixed`, the default) and the working tree (`--hard`), or
/// neither (`--soft`). With `paths`: reset just those index entries to `target` (default `HEAD`)
/// without moving `HEAD`; `--soft`/`--hard` are not allowed there.
pub async fn run(
	cwd: &Path,
	soft: bool,
	mixed: bool,
	hard: bool,
	target: Option<String>,
	paths: Vec<String>,
) -> Result<()> {
	if [soft, mixed, hard].iter().filter(|&&m| m).count() > 1 {
		bail!("--soft, --mixed, and --hard are mutually exclusive");
	}
	let rev = target.as_deref().unwrap_or("HEAD");

	if !paths.is_empty() {
		if soft || hard {
			bail!("--soft and --hard cannot be combined with paths");
		}
		// Path reset: copy the matched index entries from `rev` (the same operation as
		// `restore --staged --source=<rev>`), leaving the working tree and `HEAD` untouched.
		let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
		let repo = wt.repository();
		// Defaulting to HEAD on an unborn branch has no commit to read from; git treats the
		// source as empty there, so a matched entry is simply unstaged (removed from the index).
		let tree = if target.is_none() && repo.refs().resolve_head().await?.is_none() {
			repo.write_tree(&[]).await?
		} else {
			repo.rev_parse(&format!("{rev}^{{tree}}")).await?
		};
		let specs: Vec<&str> = paths.iter().map(String::as_str).collect();
		wt.reset_index_paths(tree, &specs, &prefix).await?;
		return Ok(());
	}

	let wt = repo::open_worktree(cwd)?;
	let repo = wt.repository();
	let commit = repo.rev_parse(&format!("{rev}^{{commit}}")).await?;

	// Materialise the index/working tree before moving the branch, so a failure (e.g. an unsafe
	// tree path) leaves `HEAD` where it was — the same order `switch` checks out before moving.
	if hard {
		// Reset the index and the working tree to the commit, discarding local changes.
		let tree = repo.commit_tree(commit).await?;
		wt.checkout(tree, true).await?;
	} else if !soft {
		// `--mixed` (the default): reset the index only.
		let tree = repo.commit_tree(commit).await?;
		wt.reset_index(tree).await?;
	}

	let committer = identity::signature_or_default(repo, "COMMITTER").await;
	repo
		.reset_head(commit, &committer, &format!("reset: moving to {rev}"))
		.await?;
	Ok(())
}
