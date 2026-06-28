use std::path::Path;

use anyhow::{Result, bail};

use crate::{commands::switch, repo};

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

	let (wt, prefix) = repo::open_worktree_with_prefix(cwd)?;
	let source = match target {
		Some(treeish) => Some(
			wt.repository()
				.rev_parse(&format!("{treeish}^{{tree}}"))
				.await?,
		),
		None => None,
	};
	let specs: Vec<&str> = paths.iter().map(String::as_str).collect();
	// `checkout -- <paths>` restores the working tree from the index; `checkout <tree> -- <paths>`
	// restores both the working tree and the index from the tree.
	wt.restore(source, true, source.is_some(), &specs, &prefix)
		.await?;
	Ok(())
}
