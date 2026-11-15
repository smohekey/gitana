use gitana_path::{GitPath, GitPathspec};

use crate::WorktreeError;

/// Why [`WorkTree::reset`](crate::WorkTree::reset) didn't reset.
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
	/// A soft reset in the middle of a merge, or with an unmerged index, which git refuses.
	#[error("cannot do a soft reset in the middle of a merge")]
	MergeInProgress,
	/// Resolving the committer for the reflog failed.
	#[error("{0:#}")]
	Identity(anyhow::Error),
	/// Resetting the index or working tree failed.
	#[error(transparent)]
	Worktree(#[from] WorktreeError),
	/// Resolving the commit, or moving the branch, failed.
	#[error(transparent)]
	Repository(#[from] gitana_repository::RepositoryError),
}

impl ResetError {
	/// The error as it displays, but with paths rendered by `render_path` and `render_pathspec`, as
	/// [`crate::SwitchError::render_with_paths`] renders them.
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
