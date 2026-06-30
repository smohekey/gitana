//! `gta revert` — record a new commit that undoes a previous commit's change.

use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::Backend;
use crate::commands::conflict::{self, ensure_trailing_newline};
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

/// Revert `commit` on the current branch, or carry an in-progress revert to its end.
///
/// Records a new single-parent commit that undoes the change `commit` introduced — a three-way merge
/// of `commit`, `HEAD`, and `commit`'s parent — authored by the current user. A conflict materialises
/// an in-progress state (`REVERT_HEAD`, `MERGE_MSG`, a conflicted index, work-tree markers) and exits
/// non-zero; resolve it and `--continue` (or `gta commit`), or `--abort` to discard it.
pub async fn run(cwd: &Path, commit: Option<String>, abort: bool, continue_: bool) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		Revert {
			commit,
			abort,
			continue_,
		},
	)
	.await
}

struct Revert {
	commit: Option<String>,
	abort: bool,
	continue_: bool,
}

impl WorkTreeCommand for Revert {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return abort_revert(&wt).await;
		}
		if self.continue_ {
			return complete(&wt, None).await;
		}

		let Some(commit) = self.commit else {
			bail!("revert requires a commit (or --abort/--continue)");
		};
		let repository = wt.repository();

		// Refuse to start a new operation before the previous one is concluded.
		if repository.revert_head().await?.is_some() {
			bail!("a revert is already in progress (REVERT_HEAD exists)");
		}
		if repository.cherry_pick_head().await?.is_some() {
			bail!("you have not concluded your cherry-pick (CHERRY_PICK_HEAD exists)");
		}
		if repository.merge_head().await?.is_some() {
			bail!("you have not concluded your merge (MERGE_HEAD exists)");
		}
		if wt.load_index()?.has_conflicts() {
			bail!("revert is not possible because you have unmerged files");
		}

		let target = repository
			.rev_parse(&format!("{commit}^{{commit}}"))
			.await?;
		let reverted = read_commit(repository, target).await?;
		if reverted.parents.len() > 1 {
			bail!(
				"commit {commit} is a merge but no mainline was given; reverting a merge is not supported"
			);
		}

		// Detached HEAD is rejected up front (the completing `commit_on_head` is symbolic-only) so a
		// clean revert cannot mutate the work tree and then fail; an unborn branch has nothing to revert.
		let head = match repository.refs().read_head().await? {
			HeadState::Symbolic(branch) => repository.refs().resolve(&branch).await?,
			HeadState::Detached(_) => bail!("cannot revert onto a detached HEAD (not yet supported)"),
		};
		let Some(head) = head else {
			bail!("cannot revert onto an unborn branch");
		};
		let head_tree = repository.commit_tree(head).await?;

		// A dirty index would be silently overwritten by the checkout below, so require it to match
		// HEAD, as git does (`git revert` refuses a dirty index).
		if conflict::index_tree(&wt).await? != head_tree {
			bail!("cannot revert: you have staged changes; commit or stash them first");
		}

		// Reverse three-way merge: roll back `commit` by merging towards its parent (an empty tree for
		// a root commit). base = the reverted commit, theirs = its parent.
		let parent_tree = match reverted.parents.first() {
			Some(parent) => repository.commit_tree(*parent).await?,
			None => repository.write_tree(&[]).await?,
		};
		let merge = repository
			.merge_trees(reverted.tree, head_tree, parent_tree)
			.await?;

		// An empty result means the change is already undone, which git refuses.
		if merge.tree == head_tree {
			bail!("the revert is empty (the change has already been reverted)");
		}

		let author = identity::signature(repository, "AUTHOR").await?;
		let committer = identity::signature(repository, "COMMITTER").await?;
		let message = revert_message(&reverted, target);

		if !merge.conflicts.is_empty() {
			conflict::write_conflicted_state(
				&wt,
				merge.tree,
				reverted.tree,
				head_tree,
				parent_tree,
				&merge.conflicts,
			)
			.await?;
			repository.set_orig_head(head).await?;
			repository.start_revert(target, &message).await?;
			return Err(conflict::report_conflicts(&merge.conflicts));
		}

		// Materialise first: a checkout that would clobber a touched local change fails before any
		// commit.
		wt.checkout(merge.tree, false).await?;
		let new_commit = repository
			.commit_on_head(merge.tree, &author, &committer, &message)
			.await?;
		println!("{new_commit}");
		Ok(())
	}
}

/// Conclude an in-progress revert: a single-parent commit from the resolved index, authored by the
/// current user. Shared by `revert --continue` (`message_override = None`, uses `MERGE_MSG`) and
/// `gta commit` during a revert. Refuses while the index has unmerged stages.
pub(crate) async fn complete<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
	message_override: Option<String>,
) -> Result<()> {
	let repository = wt.repository();
	if repository.revert_head().await?.is_none() {
		bail!("there is no revert in progress (REVERT_HEAD is missing)");
	}

	let tree = conflict::resolved_tree(wt).await?;
	// Resolving back to HEAD's content leaves nothing to commit: git refuses an empty revert (leaving
	// the state for `--abort`), rather than recording an empty commit.
	if let Some(head) = repository.refs().resolve_head().await?
		&& tree == repository.commit_tree(head).await?
	{
		bail!("the revert resolved to no change; use `gta revert --abort` to cancel");
	}
	let author = identity::signature(repository, "AUTHOR").await?;
	let committer = identity::signature(repository, "COMMITTER").await?;
	let message = match message_override {
		Some(message) => message,
		None => repository.merge_msg().await?.unwrap_or_default(),
	};
	let message = ensure_trailing_newline(message);

	let new_commit = repository
		.commit_on_head(tree, &author, &committer, &message)
		.await?;
	repository.clear_revert().await?;
	println!("{new_commit}");
	Ok(())
}

/// Abort an in-progress revert: restore the work tree and index to the (unmoved) `HEAD` and clear the
/// revert state. Like `git revert --abort`.
async fn abort_revert<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	if repository.revert_head().await?.is_none() {
		bail!("there is no revert to abort (REVERT_HEAD is missing)");
	}
	conflict::restore_to_head(wt).await?;
	repository.clear_revert().await?;
	Ok(())
}

/// git's default revert message: `Revert "<subject>"` followed by `This reverts commit <hash>.`.
fn revert_message<H: HashAlgorithm>(reverted: &Commit<H>, target: ObjectId<H>) -> String {
	let subject = reverted.message.lines().next().unwrap_or("");
	format!("Revert \"{subject}\"\n\nThis reverts commit {target}.\n")
}

/// Read and parse a commit object.
async fn read_commit<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
	oid: ObjectId<H>,
) -> Result<Commit<H>> {
	let (_, payload) = repository.objects().read_object(&oid).await?;
	Ok(parse_commit::<H>(&payload)?)
}
