use std::path::Path;

use anyhow::Result;
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// List the paths tracked in the index (stage 0), one per line.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_worktree(cwd, LsFiles).await
}

struct LsFiles;

impl WorkTreeCommand for LsFiles {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<LocalFileStore, H>,
		_prefix: String,
	) -> Result<()> {
		let index = worktree.load_index()?;
		for entry in index.entries.iter().filter(|e| e.stage == 0) {
			println!("{}", entry.path);
		}
		Ok(())
	}
}
