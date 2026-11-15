use gitana_path::{GitPath, GitPathspec};

/// Why [`crate::Repository::create_branch`] made no branch.
#[derive(Debug, thiserror::Error)]
pub enum BranchError {
	/// A branch of that name exists already.
	#[error("a branch named '{0}' already exists")]
	Exists(crate::BranchName),
	/// Resolving the committer for the branch's reflog entry failed.
	#[error("{0:#}")]
	Identity(anyhow::Error),
	/// Resolving the start point, or writing the ref, failed.
	#[error(transparent)]
	Repository(#[from] crate::RepositoryError),
}

impl BranchError {
	/// The error as it displays, but with a repository failure's paths rendered by `render_path`
	/// and `render_pathspec`, as [`crate::RepositoryError::render_with_paths`] renders them, so two
	/// paths that display alike (a raw byte, and its escaped spelling) stay told apart.
	pub fn render_with_paths(
		&self,
		render_path: impl Fn(&GitPath) -> String,
		render_pathspec: impl Fn(&GitPathspec) -> String,
	) -> String {
		match self {
			Self::Repository(error) => error.render_with_paths(render_path, render_pathspec),
			other => other.to_string(),
		}
	}
}
