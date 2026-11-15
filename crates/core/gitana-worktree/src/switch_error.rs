use gitana_path::{GitPath, GitPathspec};

use crate::WorktreeError;

/// Why [`WorkTree::switch_branch`](crate::WorkTree::switch_branch) didn't switch. Nothing of the
/// working tree, index or `HEAD` changed, but for a checkout that failed part-way (`Worktree`).
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
	/// No local branch of that name.
	#[error("invalid reference: {0}")]
	NoSuchBranch(gitana_repository::BranchName),
	/// Making the branch (`create`): one of that name exists already.
	#[error("a branch named '{0}' already exists")]
	Exists(gitana_repository::BranchName),
	/// The branch is checked out in another of the repository's worktrees.
	#[error("'{branch}' is already checked out at '{place}'")]
	CheckedOutElsewhere {
		branch: gitana_repository::BranchName,
		place: String,
	},
	/// A merge, cherry-pick, revert or rebase is in progress.
	#[error("cannot switch branch while a {0} is in progress")]
	InProgress(gitana_repository::Operation),
	/// Resolving the committer for the reflog failed.
	#[error("{0:#}")]
	Identity(anyhow::Error),
	/// The checkout refused (a local change, an untracked or ignored file in the way, an unmerged
	/// index) or failed.
	#[error(transparent)]
	Worktree(#[from] WorktreeError),
	/// Reading or writing the repository failed.
	#[error(transparent)]
	Repository(#[from] gitana_repository::RepositoryError),
}

impl SwitchError {
	/// The error as it displays, but with a checkout's or repository's paths rendered by
	/// `render_path` and `render_pathspec`, as [`WorktreeError::render_with_paths`] renders them,
	/// so two paths that display alike (a raw byte, and its escaped spelling) stay told apart.
	pub fn render_with_paths(
		&self,
		render_path: impl Fn(&GitPath) -> String,
		render_pathspec: impl Fn(&GitPathspec) -> String,
	) -> String {
		match self {
			Self::Worktree(error) => error.render_with_paths(render_path, render_pathspec),
			Self::Repository(error) => error.render_with_paths(render_path, render_pathspec),
			other => other.to_string(),
		}
	}
}
