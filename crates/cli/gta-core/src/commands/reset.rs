use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

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
	dispatch::on_worktree(
		cwd,
		Reset {
			soft,
			mixed,
			hard,
			target,
			paths,
		},
	)
	.await
}

struct Reset {
	soft: bool,
	mixed: bool,
	hard: bool,
	target: Option<String>,
	paths: Vec<String>,
}

impl WorkTreeCommand for Reset {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		if [self.soft, self.mixed, self.hard]
			.iter()
			.filter(|&&m| m)
			.count()
			> 1
		{
			bail!("--soft, --mixed, and --hard are mutually exclusive");
		}
		let rev = self.target.as_deref().unwrap_or("HEAD");
		let repo = worktree.repository();

		if !self.paths.is_empty() {
			if self.soft || self.hard {
				bail!("--soft and --hard cannot be combined with paths");
			}
			// Path reset: copy the matched index entries from `rev` (the same operation as
			// `restore --staged --source=<rev>`), leaving the working tree and `HEAD` untouched.
			//
			// Defaulting to HEAD on an unborn branch has no commit to read from; git treats the
			// source as empty there, so a matched entry is simply unstaged (removed from the index).
			let tree = if self.target.is_none() && repo.refs().resolve_head().await?.is_none() {
				repo.write_tree(&[]).await?
			} else {
				repo.rev_parse(&format!("{rev}^{{tree}}")).await?
			};
			let specs: Vec<&str> = self.paths.iter().map(String::as_str).collect();
			worktree.reset_index_paths(tree, &specs, &prefix).await?;
			return Ok(());
		}

		let commit = repo.rev_parse(&format!("{rev}^{{commit}}")).await?;

		// Materialise the index/working tree before moving the branch, so a failure (e.g. an unsafe
		// tree path) leaves `HEAD` where it was — the same order `switch` checks out before moving.
		if self.hard {
			// Reset the index and the working tree to the commit, discarding local changes.
			let tree = repo.commit_tree(commit).await?;
			worktree.checkout(tree, true).await?;
		} else if !self.soft {
			// `--mixed` (the default): reset the index only.
			let tree = repo.commit_tree(commit).await?;
			worktree.reset_index(tree).await?;
		}

		let committer = identity::signature_or_default(repo, "COMMITTER").await;
		repo
			.reset_head(commit, &committer, &format!("reset: moving to {rev}"))
			.await?;
		Ok(())
	}
}
