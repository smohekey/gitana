//! `reset <commit>` in its three modes: move the current branch (or a detached `HEAD`) to a commit,
//! then reset the index (`--mixed`), the index and working tree (`--hard`), or neither (`--soft`).

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_identity::Identity;
use gitana_object::HashAlgorithm;

use crate::{ResetError, ResetMode, ResetOutcome, WorkTree};

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	target: &str,
	mode: ResetMode,
	identity: &impl Identity,
	excludes_file: Option<&[u8]>,
) -> Result<ResetOutcome<H>, ResetError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	// git refuses a soft reset in the middle of a merge, or with an unmerged index: the merge's
	// result would be lost from the index while its state stayed.
	if mode == ResetMode::Soft
		&& (repo.merge_head().await?.is_some() || wt.load_index().await?.has_conflicts())
	{
		return Err(ResetError::MergeInProgress);
	}
	let named = repo.rev_parse(target).await?;
	let commit = repo.peel_to_commit(named).await?;
	let tree = repo.commit_tree(commit).await?;
	let committer = identity
		.committer_or_default()
		.await
		.map_err(ResetError::Identity)?;
	// Shaped as merge is: the history lease and `HEAD`'s locks taken, a merge's autostash saved and
	// `HEAD`'s move checked, all before the index or working tree changes, so a failure until then
	// changes nothing and nothing moves `HEAD` meanwhile. Then the index and working tree; then
	// `HEAD` moves and, as git's reset does whatever its mode, an in-progress merge, cherry-pick or
	// revert is abandoned (a rebase's is left to `rebase --abort`).
	let prepared = repo
		.prepare_reset_abandoning_operations(commit, &committer, &format!("reset: moving to {target}"))
		.await?;
	match mode {
		ResetMode::Hard => wt.checkout(tree, true, excludes_file).await?,
		ResetMode::Mixed => wt.reset_index(tree).await?,
		ResetMode::Soft => {}
	}
	let autostash = prepared.finish().await?;
	Ok(ResetOutcome { commit, autostash })
}
