use std::{
	io::Write,
	path::{Path, PathBuf},
};

use crate::{Backend, PathQuoteMode};
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};

/// Print the working-tree status in `git status --porcelain=v1` form.
pub async fn run(cwd: &Path, quote_path: PathQuoteMode) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Status {
			cwd: cwd.to_owned(),
			quote_path,
		},
	)
	.await
}

struct Status {
	cwd: PathBuf,
	quote_path: PathQuoteMode,
}

impl WorkTreeCommand for Status {
	/// The directory the command runs in once the repository is held: the work tree's top when run
	/// from where `.git` is while `core.worktree` puts the work tree elsewhere, as git changes into it,
	/// so a relative excludes file is found there.
	fn set_command_directory(&mut self, path: std::path::PathBuf, _directory: cap_std::fs::Dir) {
		self.cwd = path;
	}

	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		// git consults its global excludes file (`core.excludesFile`) for untracked detection; it lives
		// outside the worktree, so resolve its content here and pass it in. `core.ignoreCase` and
		// `.git/info/exclude` are read inside the worktree crate.
		let config = worktree.repository().effective_config().await?;
		let configured_quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let quote_path = match self.quote_path {
			PathQuoteMode::Config => configured_quote_path,
			PathQuoteMode::Always => true,
		};
		let excludes_file = crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;
		let status = worktree.status(excludes_file.as_deref()).await?;
		let mut stdout = std::io::stdout().lock();
		stdout.write_all(&status.porcelain_v1_bytes(quote_path))?;
		stdout.flush()?;
		Ok(())
	}
}
