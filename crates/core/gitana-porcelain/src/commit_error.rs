//! The typed failure of [`commit`](super::commit).

use std::error::Error;
use std::fmt;

use gitana_repository::RepositoryError;
use gitana_worktree::WorktreeError;

/// Why a [`commit`](super::commit) did not record a commit.
///
/// The three refusal variants are *invalid-input* conditions git reports before touching the object
/// store (an unmerged, empty, or unchanged index); the remaining variants carry the typed underlying
/// failure. Keeping the underlying error typed — rather than erasing it to `anyhow::Error` — lets a
/// caller that surfaces a structured error (the wasm component's `repo-error`) preserve the precise
/// kind: a corrupt index or object stays `corruption`, a losing branch compare-and-set stays
/// `ref-moved`, and so on. `gta-core` propagates it as an `anyhow::Error` (via the `std::error::Error`
/// impl), so its CLI message is unchanged.
#[derive(Debug)]
pub enum CommitError {
	/// The index has unmerged (conflicted) paths; they must be resolved first.
	Unmerged,
	/// Nothing is staged — the index is empty.
	Empty,
	/// The staged tree matches `HEAD`; there is no change to record.
	NothingToCommit,
	/// Resolving the author/committer identity failed.
	Identity(anyhow::Error),
	/// Signing the commit failed (`gta commit -S`) — the signer (e.g. `ssh-keygen`) errored.
	Signing(anyhow::Error),
	/// Reading the index failed.
	Index(WorktreeError),
	/// A repository operation failed — writing the tree, reading `HEAD`, or writing the commit.
	Repository(RepositoryError),
}

impl fmt::Display for CommitError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			CommitError::Unmerged => f.write_str(
				"committing is not possible because you have unmerged files; resolve them and mark \
				 resolution with `gta add`/`gta rm`",
			),
			CommitError::Empty => f.write_str("nothing to commit (empty index)"),
			CommitError::NothingToCommit => f.write_str("nothing to commit, working tree clean"),
			CommitError::Identity(error) => write!(f, "{error:#}"),
			CommitError::Signing(error) => write!(f, "{error:#}"),
			CommitError::Index(error) => write!(f, "{error}"),
			CommitError::Repository(error) => write!(f, "{error}"),
		}
	}
}

impl Error for CommitError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			CommitError::Index(error) => Some(error),
			CommitError::Repository(error) => Some(error),
			// `anyhow::Error` is not a `std::error::Error`, so its chain is folded into `Display` instead.
			_ => None,
		}
	}
}
