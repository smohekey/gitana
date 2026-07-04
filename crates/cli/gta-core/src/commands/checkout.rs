use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::commands::switch;
use crate::dispatch::{self, WorkTreeCommand};

/// `checkout` in two modes. With no `paths`, switch to branch `target` (moving `HEAD`),
/// identical to `switch`; `force` discards local changes that would be overwritten. With
/// `paths`, restore them into the working tree without moving `HEAD` — from `target` as a
/// tree-ish (also updating the index) when given, otherwise from the current index. Path
/// restore always discards uncommitted changes to those paths, so `force` does not apply.
pub async fn run(
	cwd: &Path,
	force: bool,
	target: Option<String>,
	paths: Vec<String>,
) -> Result<()> {
	if paths.is_empty() {
		let Some(name) = target else {
			bail!("missing branch to switch to, or paths to restore after `--`");
		};
		return switch::run(cwd, &name, false, None, force).await;
	}

	dispatch::on_worktree(cwd, Checkout { target, paths }).await
}

struct Checkout {
	target: Option<String>,
	paths: Vec<String>,
}

impl WorkTreeCommand for Checkout {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		let source = match self.target {
			Some(treeish) => Some(
				worktree
					.repository()
					.rev_parse(&format!("{treeish}^{{tree}}"))
					.await?,
			),
			None => None,
		};
		let specs: Vec<&str> = self.paths.iter().map(String::as_str).collect();
		// `checkout -- <paths>` restores the working tree from the index; `checkout <tree> -- <paths>`
		// restores both the working tree and the index from the tree.
		worktree
			.restore(source, true, source.is_some(), &specs, &prefix)
			.await?;
		Ok(())
	}
}
