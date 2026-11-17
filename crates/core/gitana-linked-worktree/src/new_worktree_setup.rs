use std::error::Error;
use std::future::Future;
use std::path::Path;

use crate::Setup;

/// How the caller resolves its git setup for a worktree [`create`](crate::create) is making, once the
/// worktree's git folder and `HEAD` exist. git checks a new worktree out in a child process run in
/// it, so the config's `onbranch:` and `gitdir:` conditions, and the attributes files the config
/// names, follow the new worktree rather than the one the command ran in.
pub trait NewWorktreeSetup: Send + Sync {
	/// The setup for the new worktree whose git folder (its `HEAD` written) is `git_dir`, and whose
	/// files go in `worktree`.
	fn setup_for(
		&self,
		git_dir: &Path,
		worktree: &Path,
	) -> impl Future<Output = Result<Setup, Box<dyn Error + Send + Sync>>> + Send;
}

/// One setup everywhere: the new worktree's is the same as the caller's.
impl NewWorktreeSetup for Setup {
	fn setup_for(
		&self,
		_git_dir: &Path,
		_worktree: &Path,
	) -> impl Future<Output = Result<Setup, Box<dyn Error + Send + Sync>>> + Send {
		std::future::ready(Ok(self.clone()))
	}
}
