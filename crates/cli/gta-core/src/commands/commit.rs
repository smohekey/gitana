use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use gitana_repository::{FileMode, TreeBuildEntry};

use crate::repo::{self, LocalRepository};

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
	let author = signature(repo, "AUTHOR").await?;
	let committer = signature(repo, "COMMITTER").await?;
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

/// Build a git identity line for `role` (`AUTHOR` or `COMMITTER`) from the
/// `GIT_<role>_*` environment, falling back to `user.name`/`user.email` in config.
async fn signature(repo: &LocalRepository, role: &str) -> Result<String> {
	let config = repo.read_config().await.ok();
	let from_config = |key: &str| {
		config
			.as_ref()
			.and_then(|c| c.get_string("user", None, key).map(str::to_owned))
	};
	let name = std::env::var(format!("GIT_{role}_NAME"))
		.ok()
		.or_else(|| from_config("name"))
		.with_context(|| format!("identity name not set (GIT_{role}_NAME or user.name)"))?;
	let email = std::env::var(format!("GIT_{role}_EMAIL"))
		.ok()
		.or_else(|| from_config("email"))
		.with_context(|| format!("identity email not set (GIT_{role}_EMAIL or user.email)"))?;
	let date = std::env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| now());
	Ok(format!("{name} <{email}> {date}"))
}

fn now() -> String {
	let secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);
	format!("{secs} +0000")
}
