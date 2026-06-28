use std::path::Path;

use anyhow::{Result, bail};
use gitana_repository::{FileMode, TreeBuildEntry};

use crate::identity;
use crate::repo;

/// Create a commit from the index on the current branch.
pub async fn run(cwd: &Path, message: &str) -> Result<()> {
	let wt = repo::open_worktree(cwd)?;
	let index = wt.load_index()?;
	let entries: Vec<TreeBuildEntry> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| TreeBuildEntry {
			path: e.path.clone(),
			mode: file_mode(e.mode),
			id: e.oid,
		})
		.collect();
	if entries.is_empty() {
		bail!("nothing to commit (empty index)");
	}

	let repo = wt.repository();
	let tree = repo.write_tree(&entries).await?;
	let author = identity::signature(repo, "AUTHOR").await?;
	let committer = identity::signature(repo, "COMMITTER").await?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	let commit = repo
		.commit_on_head(tree, &author, &committer, &message)
		.await?;
	println!("{commit}");
	Ok(())
}

fn file_mode(mode: u32) -> FileMode {
	match mode {
		0o100755 => FileMode::Executable,
		0o120000 => FileMode::Symlink,
		_ => FileMode::Regular,
	}
}
