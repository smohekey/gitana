//! The shared conflict lifecycle for the merge-like operations (merge, cherry-pick, revert, rebase):
//! detecting an in-progress operation, materialising a conflicted work tree and index, capturing the
//! resolved tree, and restoring the work tree on abort. Operations record this state and report the
//! conflicted paths as data; the CLI adapter renders the `CONFLICT` lines and decides the process's
//! fate (printing and the non-zero exit are policy, not engine).

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;
use gitana_repository::Repository;
use gitana_worktree::{WorkTree, WorktreeError};

pub use gitana_worktree::ConflictOverwriteError;

/// The merge-like operation currently in progress (`merge` / `cherry-pick` / `revert` / `rebase`), or
/// `None` when the work tree is idle. The history-editing operations call this before starting so that
/// only one is ever underway, and each is concluded (`--continue`) or discarded (`--abort`) before
/// another begins.
pub async fn operation_in_progress<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
) -> Result<Option<&'static str>> {
	Ok(
		repository
			.operation_in_progress()
			.await?
			.map(|operation| operation.as_str()),
	)
}

/// Write the merged result to the work tree (conflicted files carry markers) and record the conflict
/// stages (1/2/3 from base/ours/theirs) in the index. Refuses — before any caller records operation
/// state — if the checkout would clobber a touched local change.
pub async fn write_conflicted_state<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	ours_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	conflicts: &[GitPath],
) -> Result<()> {
	write_conflicted_state_with_excludes(
		wt,
		merged_tree,
		base_tree,
		ours_tree,
		theirs_tree,
		conflicts,
		None,
	)
	.await
}

/// [`write_conflicted_state`] with caller-resolved global excludes for checkout obstruction checks.
/// The worktree's own refusal comes back as a bare [`ConflictOverwriteError`], which frontends
/// find in the error's chain.
pub(crate) async fn write_conflicted_state_with_excludes<
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
>(
	wt: &WorkTree<F, W, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	ours_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	conflicts: &[GitPath],
	excludes_file: Option<&[u8]>,
) -> Result<()> {
	match wt
		.write_conflicted_state(
			merged_tree,
			base_tree,
			ours_tree,
			theirs_tree,
			conflicts,
			excludes_file,
		)
		.await
	{
		Ok(()) => Ok(()),
		Err(WorktreeError::ConflictOverwrite(refusal)) => Err(anyhow::Error::new(refusal)),
		Err(other) => Err(other.into()),
	}
}

/// The tree captured by the resolved index, refusing while unmerged stages remain. An empty index is
/// valid here (e.g. a delete/modify conflict resolved by deletion): `write_tree(&[])` is an empty
/// tree, unlike an ordinary commit which rejects it.
pub async fn resolved_tree<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<ObjectId<H>> {
	let index = wt.load_index().await?;
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}
	let entries = index.tree_entries();
	Ok(wt.repository().write_tree(&entries).await?)
}

/// The tree the index currently records (stage-0 entries only), assuming no unmerged stages. Used
/// to require a clean index before starting an operation (the index must equal `HEAD`).
pub async fn index_tree<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<ObjectId<H>> {
	let entries = wt.load_index().await?.tree_entries();
	Ok(wt.repository().write_tree(&entries).await?)
}

/// Restore the work tree and index to the (unmoved) `HEAD`, discarding conflict markers and unmerged
/// stages — the shared core of `--abort`. The caller clears its own operation state afterwards.
pub async fn restore_to_head<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(head) = repository.refs().resolve_head().await? else {
		bail!("HEAD is unborn");
	};
	let head_tree = repository.commit_tree(head).await?;
	wt.checkout(head_tree, true, None).await?;
	Ok(())
}

/// Ensure a commit message ends with a single trailing newline.
pub fn ensure_trailing_newline(message: String) -> String {
	if message.ends_with('\n') {
		message
	} else {
		format!("{message}\n")
	}
}
