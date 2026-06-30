use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// `restore` selected paths into the working tree and/or the index, without moving `HEAD`.
///
/// `worktree`/`staged` choose the targets (default: working tree only). The source is `--source`
/// as a tree-ish when given; otherwise the index for a worktree-only restore, or `HEAD` once the
/// index is a target — matching `git restore`'s defaults. Path restore always discards
/// uncommitted changes to the selected paths.
pub async fn run(
	cwd: &Path,
	worktree: bool,
	staged: bool,
	source: Option<String>,
	paths: Vec<String>,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Restore {
			worktree,
			staged,
			source,
			paths,
		},
	)
	.await
}

struct Restore {
	worktree: bool,
	staged: bool,
	source: Option<String>,
	paths: Vec<String>,
}

impl WorkTreeCommand for Restore {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		prefix: String,
	) -> Result<()> {
		if self.paths.is_empty() {
			bail!("you must specify path(s) to restore");
		}

		// Neither flag means the working tree, as in `git restore`.
		let restore_worktree = self.worktree || !self.staged;

		let tree = match self.source {
			Some(treeish) => Some(
				worktree
					.repository()
					.rev_parse(&format!("{treeish}^{{tree}}"))
					.await?,
			),
			// Restoring the index defaults to `HEAD`; a worktree-only restore defaults to the index.
			None if self.staged => Some(worktree.repository().rev_parse("HEAD^{tree}").await?),
			None => None,
		};

		let specs: Vec<&str> = self.paths.iter().map(String::as_str).collect();
		worktree
			.restore(tree, restore_worktree, self.staged, &specs, &prefix)
			.await?;
		Ok(())
	}
}
