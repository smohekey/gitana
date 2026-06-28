use std::path::Path;

use anyhow::Result;

use crate::repo;

/// List the paths tracked in the index (stage 0), one per line.
pub async fn run(cwd: &Path) -> Result<()> {
	let index = repo::open_worktree(cwd)?.load_index()?;
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		println!("{}", entry.path);
	}
	Ok(())
}
