use std::path::Path;

use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::Backend;
use crate::dispatch::{self, WorkTreeCommand};

/// Delete loose objects unreachable from every root (refs, HEAD, index, reflogs, in-progress ops).
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_worktree(cwd, Prune).await
}

struct Prune;

impl WorkTreeCommand for Prune {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		_prefix: String,
	) -> Result<()> {
		let report = gitana_porcelain::prune(&worktree).await?;
		println!("Pruned {} unreachable object(s).", report.pruned);
		Ok(())
	}
}
