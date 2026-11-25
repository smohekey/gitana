use std::{
	io::Write,
	path::{Path, PathBuf},
	sync::Arc,
};

use crate::{Backend, PathQuoteMode};
use anyhow::{Result, anyhow};
use gitana_object::HashAlgorithm;
use gitana_repository_layout::RepositoryLayout;
use gitana_submodule_probe::NativeSubmoduleProbe;
use gitana_worktree::{IgnoreSubmodules, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};

/// Print the working-tree status in `git status --porcelain=v1` form, looking inside submodules
/// as git does. `ignore_submodules` is `--ignore-submodules=<when>` (`none`, `untracked`, `dirty`
/// or `all`), when given.
pub async fn run(
	cwd: &Path,
	quote_path: PathQuoteMode,
	ignore_submodules: Option<String>,
) -> Result<()> {
	let ignore_submodules = ignore_submodules
		.map(|when| {
			IgnoreSubmodules::parse(&when)
				.ok_or_else(|| anyhow!("bad --ignore-submodules argument: {when}"))
		})
		.transpose()?;
	dispatch::on_worktree(
		cwd,
		Status {
			cwd: cwd.to_owned(),
			quote_path,
			ignore_submodules,
			layout: None,
		},
	)
	.await
}

struct Status {
	cwd: PathBuf,
	quote_path: PathQuoteMode,
	ignore_submodules: Option<IgnoreSubmodules>,
	layout: Option<RepositoryLayout>,
}

impl WorkTreeCommand for Status {
	/// git's status refuses a `diff.ignoreSubmodules` it can't read, whether or not there's a
	/// submodule or a flag to outrank it, before anything else (probed vs git 2.55). The library's
	/// status checks too; this keeps git's order, ahead of `core.quotePath` and the excludes file.
	fn reads_diff_settings(&self) -> bool {
		true
	}

	/// The directory the command runs in once the repository is held: the work tree's top when run
	/// from where `.git` is while `core.worktree` puts the work tree elsewhere, as git changes into it,
	/// so a relative excludes file is found there.
	fn set_command_directory(&mut self, path: std::path::PathBuf, _directory: cap_std::fs::Dir) {
		self.cwd = path;
	}

	/// Where the submodules are opened from.
	fn set_layout(&mut self, layout: &RepositoryLayout) {
		self.layout = Some(layout.clone());
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
		let layout = self
			.layout
			.ok_or_else(|| anyhow!("the repository's layout wasn't given"))?;
		let probe = Arc::new(NativeSubmoduleProbe::<H>::new(layout, config.clone()));
		let mut worktree = worktree.with_submodule_probe(probe.clone());
		if let Some(ignore) = self.ignore_submodules {
			worktree = worktree.with_ignore_submodules(ignore);
		}
		let status = worktree.status(excludes_file.as_deref()).await?;
		for warning in probe.warnings() {
			eprintln!("warning: {warning}");
		}
		let mut stdout = std::io::stdout().lock();
		stdout.write_all(&status.porcelain_v1_bytes(quote_path))?;
		stdout.flush()?;
		Ok(())
	}
}
