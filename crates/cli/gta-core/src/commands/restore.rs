use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

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
	if paths.is_empty() {
		bail!("you must specify path(s) to restore");
	}

	// Neither flag means the working tree, as in `git restore`.
	let restore_worktree = worktree || !staged;

	let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
	let tree = match source {
		Some(treeish) => Some(
			wt.repository()
				.rev_parse(&format!("{treeish}^{{tree}}"))
				.await?,
		),
		// Restoring the index defaults to `HEAD`; a worktree-only restore defaults to the index.
		None if staged => Some(wt.repository().rev_parse("HEAD^{tree}").await?),
		None => None,
	};

	let specs: Vec<&str> = paths.iter().map(String::as_str).collect();
	wt.restore(tree, restore_worktree, staged, &specs, &prefix)
		.await?;
	Ok(())
}
