//! The typed failure of [`WorkTree::amend`](crate::WorkTree::amend).

use std::error::Error;
use std::fmt;

use gitana_repository::{Operation, RepositoryError};

use crate::WorktreeError;

/// Why a [`WorkTree::amend`](crate::WorkTree::amend) did not replace the last commit.
///
/// The refusals are git's (`git commit --amend`), and `Moved`, the caller's commit no longer
/// `HEAD`'s; all are reported before anything is written. The remaining variants carry the typed
/// underlying failure, as [`CommitError`](crate::CommitError)'s do.
#[derive(Debug)]
pub enum AmendError {
	/// A merge, cherry-pick, revert or rebase is in progress (git: "You are in the middle of a merge
	/// -- cannot amend.").
	InProgress(Operation),
	/// The index has unmerged (conflicted) paths; they must be resolved first.
	Unmerged,
	/// There is no commit to amend: `HEAD`'s branch is unborn (git: "You have nothing to amend.").
	NothingToAmend,
	/// `HEAD` leads to another commit than the one the caller meant to amend, or by another way
	/// (switched to another branch, even one at the same commit): it moved since.
	Moved,
	/// The amended commit would change nothing from its parent (or, with none, hold an empty tree):
	/// git refuses it without `--allow-empty` ("doing so would make it empty").
	WouldBeEmpty,
	/// Resolving the committer identity failed.
	Identity(anyhow::Error),
	/// Reading the index failed.
	Index(WorktreeError),
	/// A repository operation failed: reading `HEAD` or the commit replaced, writing the tree or the
	/// commit, or moving the ref (a losing compare-and-set included).
	Repository(RepositoryError),
}

impl fmt::Display for AmendError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			AmendError::InProgress(operation) => {
				write!(f, "you are in the middle of a {operation}; cannot amend")
			}
			AmendError::Unmerged => f.write_str(
				"amending is not possible because you have unmerged files; resolve them and mark \
				 resolution with `gta add`/`gta rm`",
			),
			AmendError::NothingToAmend => f.write_str("you have nothing to amend"),
			AmendError::Moved => {
				f.write_str("HEAD has moved: it isn't the commit that was to be amended")
			}
			AmendError::WouldBeEmpty => {
				f.write_str("amending the most recent commit would make it empty")
			}
			AmendError::Identity(error) => write!(f, "{error:#}"),
			AmendError::Index(error) => write!(f, "{error}"),
			AmendError::Repository(error) => write!(f, "{error}"),
		}
	}
}

impl Error for AmendError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			AmendError::Index(error) => Some(error),
			AmendError::Repository(error) => Some(error),
			// `anyhow::Error` is not a `std::error::Error`, so its chain is folded into `Display` instead.
			_ => None,
		}
	}
}
