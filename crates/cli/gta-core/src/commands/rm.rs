use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// Remove tracked paths from the index and (unless `cached`) the working tree.
///
/// `force` overrides the data-safety check, `recursive` allows removing a directory's tracked
/// contents, and `dry_run` reports what would be removed without changing anything. Prints
/// `rm '<path>'` for each removed path, as git does.
pub async fn run(
	cwd: &Path,
	cached: bool,
	force: bool,
	recursive: bool,
	dry_run: bool,
	pathspecs: Vec<String>,
) -> Result<()> {
	if pathspecs.is_empty() {
		bail!("no pathspec given");
	}
	let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
	let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
	let outcome = wt
		.rm(&specs, &prefix, cached, force, recursive, dry_run)
		.await?;
	// Report the removals that did happen first, then surface a per-path failure — so the side
	// effects are visible even when a later path could not be removed.
	for path in &outcome.removed {
		println!("rm '{path}'");
	}
	if let Some(error) = outcome.failure {
		return Err(error.into());
	}
	Ok(())
}
