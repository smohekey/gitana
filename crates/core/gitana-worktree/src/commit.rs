//! `commit`: record a commit from the staged index on the current branch.

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_identity::Identity;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{CommitError, PreparedCommit, WorkTree};

/// Refuse while an operation is in progress, then an unmerged, empty, or unchanged index (as git
/// does, *before* resolving identity), build the staged tree, then resolve the author/committer and
/// normalise the message. Shared by the plain commit and porcelain's signed one, so the guard order
/// and lazy-identity behaviour are identical.
pub(crate) async fn prepare<F, W, H>(
	wt: &WorkTree<F, W, H>,
	message: &str,
	identity: &impl Identity,
) -> Result<PreparedCommit<H>, CommitError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	// A merge, cherry-pick or revert concludes with its own shape of commit, and a rebase replays its
	// own; a plain commit across one would leave its state behind. Refused first, so the caller
	// concludes or aborts it.
	if let Some(operation) = repo
		.operation_in_progress()
		.await
		.map_err(CommitError::Repository)?
	{
		return Err(CommitError::InProgress(operation));
	}
	let index = wt.load_index().await.map_err(CommitError::Index)?;
	// An unmerged index would silently drop conflicted paths (they have no stage-0 entry) from the
	// tree, so refuse — as git does — until they are resolved.
	if index.has_conflicts() {
		return Err(CommitError::Unmerged);
	}
	let entries = index.tree_entries();
	if entries.is_empty() {
		return Err(CommitError::Empty);
	}
	let tree = repo
		.write_tree(&entries)
		.await
		.map_err(CommitError::Repository)?;
	// Refuse a commit that would not change the tree — git's "nothing to commit, working tree clean".
	// The initial commit (unborn HEAD) has no parent tree to match, so it is always allowed. Checked
	// before resolving identity, so a no-op reports "nothing to commit" rather than an identity error.
	if let Some(head) = repo
		.refs()
		.resolve_head()
		.await
		.map_err(CommitError::Repository)?
		&& repo
			.commit_tree(head)
			.await
			.map_err(CommitError::Repository)?
			== tree
	{
		return Err(CommitError::NothingToCommit);
	}
	let author = identity.author().await.map_err(CommitError::Identity)?;
	let committer = identity.committer().await.map_err(CommitError::Identity)?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	Ok(PreparedCommit {
		tree,
		author,
		committer,
		message,
	})
}

/// [`prepare`], then record the commit on the current branch: the new commit's id.
pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	message: &str,
	identity: &impl Identity,
) -> Result<ObjectId<H>, CommitError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let prepared = prepare(wt, message, identity).await?;
	wt.repository()
		.commit_on_head(
			prepared.tree,
			&prepared.author,
			&prepared.committer,
			&prepared.message,
		)
		.await
		.map_err(CommitError::Repository)
}
