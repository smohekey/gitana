use std::path::Path;

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::{WorkTree, WorktreeError};

use crate::dispatch::{self, WorkTreeCommand};
use crate::error::AddAdvisory;

/// Stage the given pathspecs (files, directories, or `.`), interpreted relative to `cwd`. `force`
/// (git's `-f`/`--force`) stages explicitly-named ignored paths that would otherwise be refused.
pub async fn run(cwd: &Path, pathspecs: &[String], force: bool) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Add {
			cwd: cwd.to_owned(),
			pathspecs,
			force,
		},
	)
	.await
}

struct Add<'a> {
	cwd: std::path::PathBuf,
	pathspecs: &'a [String],
	force: bool,
}

impl WorkTreeCommand for Add<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		let specs: Vec<&str> = self.pathspecs.iter().map(String::as_str).collect();
		// git reads (and validates) advice.updateSparsePath / advice.addIgnoredFile during `add` setup —
		// before touching the index — so a malformed boolean fails the command before anything is staged, on
		// every add (probed vs git 2.50.1). Read them up front for that fail-before-staging parity, and reuse
		// them to render the advisory: `false` suppresses that block's `hint:` lines (git's default shows
		// them).
		let config = worktree.repository().effective_config().await?;
		let show_sparse_hints = config
			.get_bool("advice", None, "updateSparsePath")?
			.unwrap_or(true);
		let show_ignored_hints = config
			.get_bool("advice", None, "addIgnoredFile")?
			.unwrap_or(true);
		// git's global excludes file (`core.excludesFile`) content, resolved here as it lives outside the
		// worktree. `-f` stages ignored paths regardless, so skip the *read* then — matching git, which does
		// not consult the excludes file when forcing — but still validate the setting: git aborts `add -f`
		// on a valueless `core.excludesFile` before staging (probed vs git 2.55).
		let excludes_file = if self.force {
			crate::excludes::validate_excludes_file_setting(&config)?;
			None
		} else {
			crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?
		};
		match worktree
			.add(&specs, &prefix, self.force, excludes_file.as_deref())
			.await
		{
			Ok(()) => Ok(()),
			// git stages everything it can, then exits non-zero rendering the sparse block (for out-of-cone
			// pathspecs) and/or the ignored block (for ignored pathspecs). The staged work is already saved.
			Err(WorktreeError::PathspecAdvisory { sparse, ignored }) => {
				Err(AddAdvisory::new(sparse, ignored, show_sparse_hints, show_ignored_hints).into())
			}
			Err(other) => Err(other.into()),
		}
	}
}
