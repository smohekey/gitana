use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::BranchName;
use gitana_worktree::{SwitchError, SwitchOptions, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

/// Switch the working tree and `HEAD` to branch `name`. With `create`, make the
/// branch (at `start`, default `HEAD`) first. With `force`, overwrite local changes.
pub async fn run(
	cwd: &Path,
	name: &str,
	create: bool,
	start: Option<String>,
	force: bool,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Switch {
			cwd: cwd.to_owned(),
			name,
			create,
			start,
			force,
		},
	)
	.await
}

struct Switch<'a> {
	cwd: std::path::PathBuf,
	name: &'a str,
	create: bool,
	start: Option<String>,
	force: bool,
}

impl WorkTreeCommand for Switch<'_> {
	/// git's switch, and checkout of a branch (which runs this), refuse a bad `diff.ignoreSubmodules`
	/// (probed vs git 2.55).
	fn reads_diff_settings(&self) -> bool {
		true
	}

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
		let repo = worktree.repository();
		// git says "invalid reference" for a name it can't switch to, and "not a valid branch name"
		// for one it can't make.
		let name = match BranchName::parse(self.name) {
			Ok(name) => name,
			Err(invalid) if self.create => return Err(invalid.into()),
			Err(_) => bail!("invalid reference: {}", self.name),
		};
		// git's global excludes file (`core.excludesFile`) content, resolved for every switch: git
		// validates it (a directory is fatal) even under `--force`, which skips the overwrite
		// *protection*, not config validation.
		let config = repo.effective_config().await?;
		let excludes_file = crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;
		let options = SwitchOptions {
			create: self.create.then_some(self.start.as_deref()),
			force: self.force,
			// git overwrites ignored files in the way by default.
			overwrite_ignored: true,
			excludes_file: excludes_file.as_deref(),
		};
		match worktree
			.switch_branch(&name, &options, &CliIdentity::new(repo))
			.await
		{
			Ok(()) => {}
			// The hint names `gta {op} --abort`, but the operation may have been started by stock git
			// (a `rebase-merge/` or `rebase-apply/` rebase), whose state gta's own `--abort` doesn't
			// read, so it points at the tool that started it too.
			Err(SwitchError::InProgress(operation)) => bail!(
				"cannot switch branch while {operation} is in progress\nconclude or abort it first (\"gta {operation} --abort\", or the tool that started it)"
			),
			Err(error) => return Err(error.into()),
		}
		eprintln!("Switched to branch '{}'", self.name);
		Ok(())
	}
}
