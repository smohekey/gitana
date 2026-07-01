//! `revert` — record a new commit that undoes a previous commit's change.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::Identity;
use crate::conflict;

/// The result of starting a [`revert`].
#[derive(Debug)]
pub enum RevertOutcome<H: HashAlgorithm> {
	/// A new single-parent commit undoing the target (authored by the current user) was recorded.
	Reverted { commit: ObjectId<H> },
	/// The revert conflicted; an in-progress revert has been materialised (`REVERT_HEAD`, `MERGE_MSG`,
	/// a conflicted index and work tree). The caller renders the paths and signals failure.
	Conflict { paths: Vec<String> },
}

/// Revert `commit_spec` on the current branch.
///
/// Records a new single-parent commit that undoes the change `commit_spec` introduced — a three-way
/// merge of the commit, `HEAD`, and the commit's parent — authored by the current user.
pub async fn revert<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
	commit_spec: &str,
	identity: &impl Identity,
) -> Result<RevertOutcome<H>> {
	let repository = wt.repository();

	// Refuse to start while another history-editing operation is unconcluded, or the index is unmerged.
	if let Some(op) = conflict::operation_in_progress(repository).await? {
		bail!("a {op} is already in progress; conclude it (`--continue`) or abort it (`--abort`)");
	}
	if wt.load_index()?.has_conflicts() {
		bail!("revert is not possible because you have unmerged files");
	}

	let target = repository
		.rev_parse(&format!("{commit_spec}^{{commit}}"))
		.await?;
	let reverted = read_commit(repository, target).await?;
	if reverted.parents.len() > 1 {
		bail!(
			"commit {commit_spec} is a merge but no mainline was given; reverting a merge is not supported"
		);
	}

	// Detached HEAD is rejected up front (the completing `commit_on_head` is symbolic-only) so a clean
	// revert cannot mutate the work tree and then fail; an unborn branch has nothing to revert.
	let head = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => repository.refs().resolve(&branch).await?,
		HeadState::Detached(_) => bail!("cannot revert onto a detached HEAD (not yet supported)"),
	};
	let Some(head) = head else {
		bail!("cannot revert onto an unborn branch");
	};
	let head_tree = repository.commit_tree(head).await?;

	// A dirty index would be silently overwritten by the checkout below, so require it to match HEAD,
	// as git does (`git revert` refuses a dirty index).
	if conflict::index_tree(wt).await? != head_tree {
		bail!("cannot revert: you have staged changes; commit or stash them first");
	}

	// Reverse three-way merge: roll back `commit` by merging towards its parent (an empty tree for a
	// root commit). base = the reverted commit, theirs = its parent.
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

	let message = revert_message(&reverted, target);

	if !merge.conflicts.is_empty() {
		conflict::write_conflicted_state(
			wt,
			merge.tree,
			reverted.tree,
			head_tree,
			parent_tree,
			&merge.conflicts,
		)
		.await?;
		repository.set_orig_head(head).await?;
		repository.start_revert(target, &message).await?;
		return Ok(RevertOutcome::Conflict {
			paths: merge.conflicts,
		});
	}

	// A clean revert: resolve identity only now — a conflict above materialises without it, as git does.
	// Then materialise the tree first: a checkout that would clobber a touched local change fails before
	// any commit.
	let author = identity.author().await?;
	let committer = identity.committer().await?;
	wt.checkout(merge.tree, false).await?;
	let new_commit = repository
		.commit_on_head(merge.tree, &author, &committer, &message)
		.await?;
	Ok(RevertOutcome::Reverted { commit: new_commit })
}

/// Conclude an in-progress revert: a single-parent commit from the resolved index, authored by the
/// current user, returning the new commit id. Shared by `revert --continue` (`message_override = None`,
/// uses `MERGE_MSG`) and `gta commit` during a revert. Refuses while the index has unmerged stages.
pub async fn continue_revert<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
	message_override: Option<String>,
	identity: &impl Identity,
) -> Result<ObjectId<H>> {
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
	let author = identity.author().await?;
	let committer = identity.committer().await?;
	let message = match message_override {
		Some(message) => message,
		None => repository.merge_msg().await?.unwrap_or_default(),
	};
	let message = conflict::ensure_trailing_newline(message);

	let new_commit = repository
		.commit_on_head(tree, &author, &committer, &message)
		.await?;
	repository.clear_revert().await?;
	Ok(new_commit)
}

/// Abort an in-progress revert: restore the work tree and index to the (unmoved) `HEAD` and clear the
/// revert state. Like `git revert --abort`.
pub async fn abort_revert<F: FileStore, H: HashAlgorithm>(wt: &WorkTree<F, H>) -> Result<()> {
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
async fn read_commit<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	oid: ObjectId<H>,
) -> Result<Commit<H>> {
	let (_, payload) = repository.objects().read_object(&oid).await?;
	Ok(parse_commit::<H>(&payload)?)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::{FailingIdentity, TestIdentity, commit_file, fixture};

	#[tokio::test]
	async fn reverts_the_head_commit() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		commit_file(dir.path(), &wt, "f.txt", b"v1\n", &id).await;
		let b = commit_file(dir.path(), &wt, "f.txt", b"v2\n", &id).await;

		let outcome = revert(&wt, &b.to_hex(), &id).await.unwrap();
		let RevertOutcome::Reverted { commit } = outcome else {
			panic!("expected a clean revert");
		};
		// The revert restores the prior content.
		let repo = wt.repository();
		let tree = repo.commit_tree(commit).await.unwrap();
		let entries = repo.read_tree(tree).await.unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].0, "f.txt");
		let blob = repo.write_blob(b"v1\n").await.unwrap();
		assert_eq!(entries[0].2, blob);
		assert_eq!(repo.revert_head().await.unwrap(), None);
	}

	#[tokio::test]
	async fn conflicting_revert_materialises_revert_head() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		let b = commit_file(dir.path(), &wt, "f.txt", b"ours\n", &id).await;
		// HEAD moves on past `b`, touching the same path → reverting `b` cannot apply cleanly.
		commit_file(dir.path(), &wt, "f.txt", b"current\n", &id).await;

		let outcome = revert(&wt, &b.to_hex(), &id).await.unwrap();
		let RevertOutcome::Conflict { paths } = outcome else {
			panic!("expected a conflict");
		};
		assert_eq!(paths, vec!["f.txt".to_owned()]);
		assert_eq!(wt.repository().revert_head().await.unwrap(), Some(b));
		assert!(wt.load_index().unwrap().has_conflicts());
	}

	#[tokio::test]
	async fn conflict_materialises_without_resolving_identity() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		let b = commit_file(dir.path(), &wt, "f.txt", b"ours\n", &id).await;
		commit_file(dir.path(), &wt, "f.txt", b"current\n", &id).await;

		// A conflict records REVERT_HEAD without recording a commit, so it must not require a configured
		// identity — git materialises the conflict regardless.
		let outcome = revert(&wt, &b.to_hex(), &FailingIdentity).await.unwrap();
		assert!(matches!(outcome, RevertOutcome::Conflict { .. }));
		assert_eq!(wt.repository().revert_head().await.unwrap(), Some(b));
	}
}
