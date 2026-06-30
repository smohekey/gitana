use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Stage the given pathspecs (files, directories, or `.`), interpreted relative to `cwd`.
pub async fn run(cwd: &Path, pathspecs: &[String]) -> Result<()> {
	dispatch::on_worktree(cwd, Add { pathspecs }).await
}

struct Add<'a> {
	pathspecs: &'a [String],
}

impl WorkTreeCommand for Add<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		prefix: String,
	) -> Result<()> {
		let specs: Vec<&str> = self.pathspecs.iter().map(String::as_str).collect();
		worktree.add(&specs, &prefix).await?;
		Ok(())
	}
}
