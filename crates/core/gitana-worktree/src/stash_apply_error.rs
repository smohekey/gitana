use gitana_path::GitPath;
use gitana_repository::RepositoryError;

use crate::WorktreeError;

/// Why [`WorkTree::stash_apply`](crate::WorkTree::stash_apply) brought nothing back; nothing has
/// changed.
#[derive(Debug, thiserror::Error)]
pub enum StashApplyError {
	/// There are no stashes (git: "No stash entries found.").
	#[error("No stash entries found.")]
	NoEntries,
	/// There's no `stash@{index}` (git: "log for 'stash' only has <count> entries").
	#[error("log for 'stash' only has {count} entries")]
	OutOfRange { index: usize, count: usize },
	/// The entry names a commit that isn't shaped as a stash (git: "… is not a stash-like commit").
	#[error("'{0}' is not a stash-like commit")]
	NotStashLike(String),
	/// The index has unmerged paths (git: "cannot apply a stash in the middle of a merge").
	#[error("cannot apply a stash in the middle of a merge")]
	Unmerged,
	/// With `restore_index`, the stash's staged changes don't apply to the current index (git:
	/// "conflicts in index. Try without --index.").
	#[error("conflicts in index. Try without --index.")]
	IndexConflicts,
	/// An untracked file the stash holds is already in the working tree (git: "<path> already
	/// exists, no checkout"). Refused before anything is written, where git applies the tracked
	/// changes first.
	#[error("{0} already exists, no checkout")]
	UntrackedExists(GitPath),
	#[error(transparent)]
	Worktree(#[from] WorktreeError),
	#[error(transparent)]
	Repository(#[from] RepositoryError),
}
