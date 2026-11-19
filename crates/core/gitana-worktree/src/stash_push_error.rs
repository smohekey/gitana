use gitana_object::HashAlgorithm;
use gitana_path::{GitPath, GitPathspec};
use gitana_repository::RepositoryError;

use crate::{ResetError, StashPushed, WorktreeError};

/// Why [`WorkTree::stash_push`](crate::WorkTree::stash_push) put nothing away, or (for
/// [`StashPushError::Clean`]) didn't finish once it had.
#[derive(Debug, thiserror::Error)]
pub enum StashPushError<H: HashAlgorithm> {
	/// `HEAD` names no commit yet (git: "You do not have the initial commit yet").
	#[error("You do not have the initial commit yet")]
	NoInitialCommit,
	/// The index has unmerged paths (git prints "<path>: needs merge" for each, then "could not
	/// write index").
	#[error("could not write index")]
	Unmerged(Vec<GitPath>),
	/// Without untracked files, a pathspec that matches no tracked file (git: "pathspec … did not
	/// match any file(s) known to git").
	#[error("pathspec '{0}' did not match any file(s) known to git")]
	UnmatchedPathspec(GitPathspec),
	/// The index holds intent-to-add entries (`add -N`), every one in index order (git names the
	/// first: "Entry '<path>' not uptodate. Cannot merge.", then "Cannot save the current worktree
	/// state").
	#[error("Cannot save the current worktree state")]
	IntentToAdd(Vec<GitPath>),
	/// The identity the stash commits record couldn't be resolved.
	#[error("{0:#}")]
	Identity(anyhow::Error),
	/// Recording the reset to `HEAD` (its reflog line, `ORIG_HEAD`, an in-progress operation
	/// abandoned) failed after the stash was saved and the working tree put back.
	#[error(transparent)]
	Reset(#[from] ResetError),
	/// With untracked files, the stash was stored and the untracked files removed as far as they
	/// could be, but these couldn't (each with its reason; git: "warning: failed to remove <path>:
	/// <reason>"), so, as git stops there, the tracked files weren't put back: they, and the index,
	/// are as they were.
	#[error("the stash was saved, but {} untracked path(s) could not be removed", .not_removed.len())]
	Clean {
		/// The stash stored, with what saving it reported.
		pushed: Box<StashPushed<H>>,
		/// Each file or folder left, with why.
		not_removed: Vec<(GitPath, std::io::Error)>,
	},
	#[error(transparent)]
	Worktree(#[from] WorktreeError),
	#[error(transparent)]
	Repository(#[from] RepositoryError),
}
