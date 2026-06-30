//! `gta cherry-pick` — re-apply a commit's change onto the current branch.

use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::commands::conflict::{self, ensure_trailing_newline};
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

/// Cherry-pick `commit` onto the current branch, or carry an in-progress cherry-pick to its end.
///
/// Re-applies the change `commit` introduced — a three-way merge of its parent, `HEAD`, and `commit`
/// — as a new single-parent commit that preserves `commit`'s author. A conflict materialises an
/// in-progress state (`CHERRY_PICK_HEAD`, `MERGE_MSG`, a conflicted index, work-tree markers) and
/// exits non-zero; resolve it and `--continue` (or `gta commit`), or `--abort` to discard it.
pub async fn run(cwd: &Path, commit: Option<String>, abort: bool, continue_: bool) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		CherryPick {
			commit,
			abort,
			continue_,
		},
	)
	.await
}

struct CherryPick {
	commit: Option<String>,
	abort: bool,
	continue_: bool,
}

impl WorkTreeCommand for CherryPick {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return abort_cherry_pick(&wt).await;
		}
		if self.continue_ {
			return complete(&wt, None).await;
		}

		let Some(commit) = self.commit else {
			bail!("cherry-pick requires a commit (or --abort/--continue)");
		};
		let repository = wt.repository();

		// Refuse to start while another history-editing operation is unconcluded.
		if let Some(op) = conflict::operation_in_progress(repository).await? {
			bail!("a {op} is already in progress; conclude it (`--continue`) or abort it (`--abort`)");
		}
		if wt.load_index()?.has_conflicts() {
			bail!("cherry-pick is not possible because you have unmerged files");
		}

		let pick = repository
			.rev_parse(&format!("{commit}^{{commit}}"))
			.await?;
		let picked = read_commit(repository, pick).await?;
		if picked.parents.len() > 1 {
			bail!(
				"commit {commit} is a merge but no mainline was given; cherry-picking a merge is not supported"
			);
		}

		// The current tip is the new commit's single parent. Detached HEAD is rejected up front (the
		// completing `commit_on_head` is symbolic-only, like `gta commit`) so a clean pick cannot mutate
		// the work tree and then fail; an unborn branch has no parent to pick onto.
		let head = match repository.refs().read_head().await? {
			HeadState::Symbolic(branch) => repository.refs().resolve(&branch).await?,
			HeadState::Detached(_) => {
				bail!("cannot cherry-pick onto a detached HEAD (not yet supported)")
			}
		};
		let Some(head) = head else {
			bail!("cannot cherry-pick onto an unborn branch");
		};
		let head_tree = repository.commit_tree(head).await?;

		// A dirty index would be silently overwritten by the checkout below, so require it to match
		// HEAD, as git does (`git cherry-pick` refuses a dirty index).
		if conflict::index_tree(&wt).await? != head_tree {
			bail!("cannot cherry-pick: you have staged changes; commit or stash them first");
		}

		// Three-way merge: base = the picked commit's parent (an empty tree for a root commit).
		let base_tree = match picked.parents.first() {
			Some(parent) => repository.commit_tree(*parent).await?,
			None => repository.write_tree(&[]).await?,
		};
		let merge = repository
			.merge_trees(base_tree, head_tree, picked.tree)
			.await?;

		// An empty result (the change is already present) is not a cherry-pick, as git refuses.
		if merge.tree == head_tree {
			bail!("the cherry-pick is empty (the change is already present)");
		}

		let committer = identity::signature(repository, "COMMITTER").await?;
		let message = ensure_trailing_newline(picked.message.clone());

		if !merge.conflicts.is_empty() {
			conflict::write_conflicted_state(
				&wt,
				merge.tree,
				base_tree,
				head_tree,
				picked.tree,
				&merge.conflicts,
			)
			.await?;
			repository.set_orig_head(head).await?;
			repository.start_cherry_pick(pick, &message).await?;
			return Err(conflict::report_conflicts(&merge.conflicts));
		}

		// Materialise first: a checkout that would clobber a touched local change fails before any
		// commit.
		wt.checkout(merge.tree, false).await?;
		let new_commit = repository
			.commit_on_head(merge.tree, &picked.author, &committer, &message)
			.await?;
		println!("{new_commit}");
		Ok(())
	}
}

/// Conclude an in-progress cherry-pick: a single-parent commit from the resolved index that preserves
/// the picked commit's author. Shared by `cherry-pick --continue` (`message_override = None`, uses
/// `MERGE_MSG`) and `gta commit` during a cherry-pick. Refuses while the index has unmerged stages.
pub(crate) async fn complete<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
	message_override: Option<String>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(pick) = repository.cherry_pick_head().await? else {
		bail!("there is no cherry-pick in progress (CHERRY_PICK_HEAD is missing)");
	};

	let tree = conflict::resolved_tree(wt).await?;
	// Resolving back to HEAD's content leaves nothing to commit: git refuses an empty cherry-pick
	// (leaving the state for `--abort`), rather than recording an empty commit.
	if let Some(head) = repository.refs().resolve_head().await?
		&& tree == repository.commit_tree(head).await?
	{
		bail!("the cherry-pick resolved to no change; use `gta cherry-pick --abort` to cancel");
	}
	let picked = read_commit(repository, pick).await?;
	let committer = identity::signature(repository, "COMMITTER").await?;
	let message = match message_override {
		Some(message) => message,
		None => repository
			.merge_msg()
			.await?
			.unwrap_or_else(|| picked.message.clone()),
	};
	let message = ensure_trailing_newline(message);

	let new_commit = repository
		.commit_on_head(tree, &picked.author, &committer, &message)
		.await?;
	repository.clear_cherry_pick().await?;
	println!("{new_commit}");
	Ok(())
}

/// Abort an in-progress cherry-pick: restore the work tree and index to the (unmoved) `HEAD` and
/// clear the cherry-pick state. Like `git cherry-pick --abort`.
async fn abort_cherry_pick<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	if repository.cherry_pick_head().await?.is_none() {
		bail!("there is no cherry-pick to abort (CHERRY_PICK_HEAD is missing)");
	}
	conflict::restore_to_head(wt).await?;
	repository.clear_cherry_pick().await?;
	Ok(())
}

/// Read and parse a commit object.
async fn read_commit<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
	oid: ObjectId<H>,
) -> Result<Commit<H>> {
	let (_, payload) = repository.objects().read_object(&oid).await?;
	Ok(parse_commit::<H>(&payload)?)
}
