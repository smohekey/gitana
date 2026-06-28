use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// Move or rename tracked paths: filesystem move plus index update.
///
/// The last path is the destination; the rest are sources. `force` overwrites an existing
/// destination, and `dry_run` reports the moves without performing them. With `verbose` (or
/// `dry_run`), prints `Renaming <src> to <dst>` for each move, as git does.
pub async fn run(
	cwd: &Path,
	force: bool,
	dry_run: bool,
	verbose: bool,
	paths: Vec<String>,
) -> Result<()> {
	if paths.len() < 2 {
		bail!("must specify at least one source and a destination");
	}
	let (dest, sources) = paths.split_last().unwrap();
	let sources: Vec<&str> = sources.iter().map(String::as_str).collect();

	let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
	let moves = wt.mv(&sources, dest, &prefix, force, dry_run).await?;

	if verbose || dry_run {
		for (from, to) in &moves {
			println!("Renaming {from} to {to}");
		}
	}
	Ok(())
}
