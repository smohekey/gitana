use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// Find the best common ancestor(s) of two or more commits, or — with `--is-ancestor` — test
/// whether the first commit is an ancestor of the second.
///
/// Default prints one merge base; `--all` prints every merge base, one per line. As with git,
/// "no common ancestor" and a false `--is-ancestor` exit non-zero with no output.
pub async fn run(cwd: &Path, all: bool, is_ancestor: bool, commits: Vec<String>) -> Result<()> {
	let repo = repo::open_here(cwd)?;

	if is_ancestor {
		if all {
			bail!("--all cannot be combined with --is-ancestor");
		}
		let [ancestor, descendant] = commits.as_slice() else {
			bail!("--is-ancestor requires exactly two commits");
		};
		let ancestor = repo.rev_parse(ancestor).await?;
		let descendant = repo.rev_parse(descendant).await?;
		if repo.is_ancestor(ancestor, descendant).await? {
			return Ok(());
		}
		// Not an ancestor: exit 1 with no output, like git.
		std::process::exit(1);
	}

	if commits.len() < 2 {
		bail!("merge-base requires at least two commits");
	}
	let mut ids = Vec::with_capacity(commits.len());
	for commit in &commits {
		ids.push(repo.rev_parse(commit).await?);
	}

	// `merge_base` returns bases newest-committer-date first (git's order), so printing the first
	// matches git's single-base choice whenever the bases have distinct dates. (Among bases sharing
	// a date git's pick is unspecified; ours may differ but is an equally valid base.) Keep this
	// order rather than sorting.
	let bases = repo.merge_base(&ids).await?;
	let Some(first) = bases.first() else {
		// No common ancestor: exit 1 with no output, like git.
		std::process::exit(1);
	};
	if all {
		for base in &bases {
			println!("{base}");
		}
	} else {
		println!("{first}");
	}
	Ok(())
}
