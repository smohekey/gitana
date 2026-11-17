use std::error::Error;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_linked_worktree::{NewWorktreeSetup, Setup};

use crate::command_context::CommandContext;

/// The setup a new worktree of the repository whose common git folder is `common` is checked out
/// with, as git's child checkout resolves it there: the config stack for the worktree's own git
/// folder (its `config.worktree`, and `onbranch:`/`gitdir:` conditions on its branch and folder), and
/// the attributes files that config names. The command's own context (`-c` settings, `-C`) is taken
/// when this is made and put back while resolving, since `create` resolves in a task of its own,
/// which doesn't inherit it.
#[derive(Clone, Debug)]
pub(crate) struct NewWorktreeAt {
	common: PathBuf,
	command: Option<CommandContext>,
}

impl NewWorktreeAt {
	/// For the repository whose common git folder is `common`, in the current command's context.
	pub(crate) fn new(common: &Path) -> Self {
		NewWorktreeAt {
			common: common.to_owned(),
			command: CommandContext::current(),
		}
	}

	async fn resolve(common: PathBuf, git_dir: PathBuf, worktree: PathBuf) -> Result<Setup> {
		let open = |path: &Path| {
			Dir::open_ambient_dir(path, ambient_authority())
				.map_err(|error| anyhow!("opening {}: {error}", path.display()))
		};
		let config =
			crate::git_config::for_worktree_at(open(&common)?, open(&git_dir)?, &common, &git_dir)
				.await?;
		let attributes = crate::attributes::outside_attributes(&config, &worktree).await?;
		Ok(Setup {
			config: Some(config),
			attributes,
		})
	}
}

impl NewWorktreeSetup for NewWorktreeAt {
	fn setup_for(
		&self,
		git_dir: &Path,
		worktree: &Path,
	) -> impl Future<Output = Result<Setup, Box<dyn Error + Send + Sync>>> + Send {
		let resolved = Self::resolve(self.common.clone(), git_dir.to_owned(), worktree.to_owned());
		let command = self.command.clone();
		async move {
			let setup = match command {
				Some(command) => command.scope(resolved).await,
				None => resolved.await,
			};
			setup.map_err(Into::into)
		}
	}
}
