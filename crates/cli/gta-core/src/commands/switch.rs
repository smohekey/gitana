use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Switch the working tree and `HEAD` to branch `name`. With `create`, make the
/// branch (at `start`, default `HEAD`) first. With `force`, overwrite local changes.
pub async fn run(
	cwd: &Path,
	name: &str,
	create: bool,
	start: Option<String>,
	force: bool,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Switch {
			name,
			create,
			start,
			force,
		},
	)
	.await
}

struct Switch<'a> {
	name: &'a str,
	create: bool,
	start: Option<String>,
	force: bool,
}

impl WorkTreeCommand for Switch<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		_prefix: String,
	) -> Result<()> {
		let repo = worktree.repository();
		let branch = format!("refs/heads/{}", self.name);

		if self.create {
			if repo.refs().resolve(&branch).await?.is_some() {
				bail!("a branch named '{}' already exists", self.name);
			}
			let target = repo
				.rev_parse(self.start.as_deref().unwrap_or("HEAD"))
				.await?;
			repo.refs().update_ref(&branch, target, None).await?;
		}

		let Some(commit) = repo.refs().resolve(&branch).await? else {
			bail!("invalid reference: {}", self.name);
		};

		// A branch's ref is shared across a repository's worktrees, so git forbids checking the same
		// branch out in two of them at once (their commits would race on one ref). Refuse before
		// touching the working tree, as git does.
		if let Some(other) = crate::repo::branch_checked_out_elsewhere(worktree.git_dir(), &branch) {
			bail!(
				"'{}' is already checked out at '{}'",
				self.name,
				other.display()
			);
		}

		let tree = repo.commit_tree(commit).await?;
		worktree.checkout(tree, self.force).await?;
		worktree
			.repository()
			.refs()
			.set_head_symbolic(&branch)
			.await?;
		eprintln!("Switched to branch '{}'", self.name);
		Ok(())
	}
}
