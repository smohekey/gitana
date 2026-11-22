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
/// the attributes files that config names. The command's own context (`-c` settings, `-C`) and the
/// environment override in force, if any, are taken when this is made and put back while resolving,
/// since `create` resolves in a task of its own, which inherits neither.
#[derive(Clone, Debug)]
pub(crate) struct NewWorktreeAt {
	common: PathBuf,
	command: Option<CommandContext>,
	environment: Option<gitana_config_native::Environment>,
}

impl NewWorktreeAt {
	/// For the repository whose common git folder is `common`, in the current command's context.
	pub(crate) fn new(common: &Path) -> Self {
		NewWorktreeAt {
			common: common.to_owned(),
			command: CommandContext::current(),
			environment: gitana_config_native::current_environment(),
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
		let environment = self.environment.clone();
		async move {
			let resolving = async move {
				match command {
					Some(command) => command.scope(resolved).await,
					None => resolved.await,
				}
			};
			let setup = match environment {
				Some(environment) => gitana_config_native::with_environment(environment, resolving).await,
				None => resolving.await,
			};
			setup.map_err(Into::into)
		}
	}
}

#[cfg(test)]
mod tests {
	use std::process::Command;

	/// Under an environment override, a new worktree is checked out with the override's attributes
	/// (here its XDG file's `eol=crlf`), though `create` resolves its setup in a task of its own.
	/// (The process's own attributes file isn't expected to say so.)
	#[tokio::test]
	async fn a_new_worktree_is_set_up_under_the_override_in_force() {
		let temporary = tempfile::tempdir().unwrap();
		let top = temporary.path().canonicalize().unwrap();
		let global = top.join("global");
		std::fs::write(&global, "").unwrap();
		let git = |args: &[&str]| {
			let output = Command::new("git")
				.env("GIT_CONFIG_GLOBAL", &global)
				.env("GIT_CONFIG_NOSYSTEM", "1")
				.args([
					"-c",
					"user.name=T",
					"-c",
					"user.email=t@t",
					"-c",
					"init.defaultBranch=main",
				])
				.arg("-C")
				.arg(top.join("repo"))
				.args(args)
				.output()
				.unwrap();
			assert!(
				output.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&output.stderr)
			);
		};
		std::fs::create_dir(top.join("repo")).unwrap();
		git(&["init", "-q"]);
		std::fs::write(top.join("repo/a.txt"), "a\nb\n").unwrap();
		git(&["add", "a.txt"]);
		git(&["commit", "-q", "-m", "A"]);
		let xdg = top.join("xdg");
		std::fs::create_dir_all(xdg.join("git")).unwrap();
		std::fs::write(xdg.join("git/attributes"), "*.txt text eol=crlf\n").unwrap();
		let environment = gitana_config_native::Environment {
			home: Some(top.join("home")),
			xdg_config_home: Some(xdg),
			config_global: Some(global.clone()),
			config_nosystem: true,
			attr_nosystem: true,
		};
		let action = crate::commands::worktree::Action::Add {
			path: top.join("linked"),
			commit_ish: None,
			branch: Some("linked".into()),
			force_branch: false,
			detach: false,
		};
		gitana_config_native::with_environment(
			environment,
			crate::commands::worktree::run(&top.join("repo"), action),
		)
		.await
		.unwrap();
		assert_eq!(
			std::fs::read(top.join("linked/a.txt")).unwrap(),
			b"a\r\nb\r\n"
		);
	}
}
