use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_path::GitPathspec;
use gitana_worktree::{ResetMode, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;

/// `reset` in its two forms.
///
/// Without `paths`: move the current branch (or detached `HEAD`) to `target` (default `HEAD`),
/// then — by mode — reset the index (`--mixed`, the default) and the working tree (`--hard`), or
/// neither (`--soft`). With `paths`: reset just those index entries to `target` (default `HEAD`)
/// without moving `HEAD`; `--soft`/`--hard` are not allowed there.
pub async fn run(
	cwd: &Path,
	soft: bool,
	mixed: bool,
	hard: bool,
	target: Option<Vec<u8>>,
	paths: Vec<GitPathspec>,
) -> Result<()> {
	let command = Reset {
		cwd: cwd.to_owned(),
		soft,
		mixed,
		hard,
		target,
		paths,
	};
	if command.paths.is_empty() {
		dispatch::on_worktree_history_mutation(cwd, command).await
	} else {
		dispatch::on_worktree(cwd, command).await
	}
}

struct Reset {
	cwd: std::path::PathBuf,
	soft: bool,
	mixed: bool,
	hard: bool,
	target: Option<Vec<u8>>,
	paths: Vec<GitPathspec>,
}

impl WorkTreeCommand for Reset {
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
		if [self.soft, self.mixed, self.hard]
			.iter()
			.filter(|&&m| m)
			.count()
			> 1
		{
			bail!("--soft, --mixed, and --hard are mutually exclusive");
		}
		let rev = self.target.as_deref().unwrap_or(b"HEAD");
		let repo = worktree.repository();

		if !self.paths.is_empty() {
			if self.soft || self.hard {
				bail!("--soft and --hard cannot be combined with paths");
			}
			// Path reset: copy the matched index entries from `rev` (the same operation as
			// `restore --staged --source=<rev>`), leaving the working tree and `HEAD` untouched.
			//
			// Defaulting to HEAD on an unborn branch has no commit to read from; git treats the
			// source as empty there, so a matched entry is simply unstaged (removed from the index).
			let tree = if self.target.is_none() && repo.refs().resolve_head().await?.is_none() {
				repo.write_tree(&[]).await?
			} else {
				let target = repo.rev_parse(rev).await?;
				repo.peel_to_tree(target).await?
			};
			worktree
				.reset_index_pathspecs(tree, &self.paths, &prefix)
				.await?;
			return Ok(());
		}

		let mode = if self.hard {
			ResetMode::Hard
		} else if self.soft {
			ResetMode::Soft
		} else {
			ResetMode::Mixed
		};
		// git still validates `core.excludesFile` (a directory is fatal) for a hard reset, though the
		// forced checkout skips the overwrite guard, so it's resolved and passed.
		let excludes_file = if self.hard {
			let config = repo.effective_config().await?;
			crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?
		} else {
			None
		};
		// The revision as named, for the reflog; one whose name isn't UTF-8 is resolved to its id
		// first, as the reset takes a name.
		let rev = match std::str::from_utf8(rev) {
			Ok(rev) => rev.to_owned(),
			Err(_) => repo.rev_parse(rev).await?.to_hex(),
		};
		let outcome = worktree
			.reset(
				&rev,
				mode,
				&CliIdentity::new(repo),
				excludes_file.as_deref(),
			)
			.await?;
		// As git tells it when its reset saves a merge's autostash.
		if outcome.autostash.is_some() {
			eprintln!("Autostash exists; creating a new stash entry.");
			eprintln!("Your changes are safe in the stash.");
			eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
		}
		Ok(())
	}
}
