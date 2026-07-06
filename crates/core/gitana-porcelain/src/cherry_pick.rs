//! `cherry-pick` — re-apply a commit's change onto the current branch.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::conflict;
use crate::{Identity, Signer, signing};

/// The result of starting a [`cherry_pick`].
#[derive(Debug)]
pub enum PickOutcome<H: HashAlgorithm> {
	/// A new single-parent commit (preserving the picked commit's author) was recorded.
	Picked { commit: ObjectId<H> },
	/// The pick conflicted; an in-progress cherry-pick has been materialised (`CHERRY_PICK_HEAD`,
	/// `MERGE_MSG`, a conflicted index and work tree). The caller renders the paths and signals failure.
	Conflict { paths: Vec<String> },
}

/// Cherry-pick `commit_spec` onto the current branch.
///
/// Re-applies the change `commit_spec` introduced — a three-way merge of its parent, `HEAD`, and the
/// commit — as a new single-parent commit preserving the picked author.
pub async fn cherry_pick<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	commit_spec: &str,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<PickOutcome<H>> {
	let repository = wt.repository();

	// Refuse to start while another history-editing operation is unconcluded, or the index is unmerged.
	if let Some(op) = conflict::operation_in_progress(repository).await? {
		bail!("a {op} is already in progress; conclude it (`--continue`) or abort it (`--abort`)");
	}
	if wt.load_index().await?.has_conflicts() {
		bail!("cherry-pick is not possible because you have unmerged files");
	}

	let pick = repository
		.rev_parse(&format!("{commit_spec}^{{commit}}"))
		.await?;
	let picked = read_commit(repository, pick).await?;
	if picked.parents.len() > 1 {
		bail!(
			"commit {commit_spec} is a merge but no mainline was given; cherry-picking a merge is not supported"
		);
	}

	// The current tip is the new commit's single parent. Detached HEAD is rejected up front (recording
	// the commit is symbolic-only, like `gta commit`) so a clean pick cannot mutate the work tree and
	// then fail; an unborn branch has no parent to pick onto.
	let branch = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => {
			bail!("cannot cherry-pick onto a detached HEAD (not yet supported)")
		}
	};
	let Some(head) = repository.refs().resolve(&branch).await? else {
		bail!("cannot cherry-pick onto an unborn branch");
	};
	let head_tree = repository.commit_tree(head).await?;

	// A dirty index would be silently overwritten by the checkout below, so require it to match HEAD,
	// as git does (`git cherry-pick` refuses a dirty index).
	if conflict::index_tree(wt).await? != head_tree {
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

	let message = conflict::ensure_trailing_newline(picked.message.clone());

	if !merge.conflicts.is_empty() {
		conflict::write_conflicted_state(
			wt,
			merge.tree,
			base_tree,
			head_tree,
			picked.tree,
			&merge.conflicts,
		)
		.await?;
		repository.set_orig_head(head).await?;
		repository.start_cherry_pick(pick, &message).await?;
		return Ok(PickOutcome::Conflict {
			paths: merge.conflicts,
		});
	}

	// A clean pick: resolve identity only now — a conflict above materialises without it, as git does.
	// Build (and, when configured, sign) the commit *before* the checkout: signing can fail, and the
	// object write is inert until a ref names it, so a failure leaves the work tree untouched. Then
	// materialise (a clobbering checkout fails here, before the ref moves) and advance the branch.
	let committer = identity.committer().await?;
	let new_commit = signing::seal_commit(
		repository,
		merge.tree,
		vec![head],
		&picked.author,
		&committer,
		&message,
		signer,
	)
	.await?;
	wt.checkout(merge.tree, false).await?;
	repository
		.record_commit(&branch, Some(head), new_commit, &committer, &message)
		.await?;
	Ok(PickOutcome::Picked { commit: new_commit })
}

/// Conclude an in-progress cherry-pick: a single-parent commit from the resolved index that preserves
/// the picked commit's author, returning the new commit id. Shared by `cherry-pick --continue`
/// (`message_override = None`, uses `MERGE_MSG`) and `gta commit` during a cherry-pick. Refuses while
/// the index has unmerged stages.
pub async fn continue_cherry_pick<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	message_override: Option<String>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<ObjectId<H>> {
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
	let committer = identity.committer().await?;
	let message = match message_override {
		Some(message) => message,
		None => repository
			.merge_msg()
			.await?
			.unwrap_or_else(|| picked.message.clone()),
	};
	let message = conflict::ensure_trailing_newline(message);

	let new_commit = signing::commit_on_head(
		repository,
		tree,
		&picked.author,
		&committer,
		&message,
		signer,
	)
	.await?;
	repository.clear_cherry_pick().await?;
	Ok(new_commit)
}

/// Abort an in-progress cherry-pick: restore the work tree and index to the (unmoved) `HEAD` and clear
/// the cherry-pick state. Like `git cherry-pick --abort`.
pub async fn abort_cherry_pick<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<()> {
	let repository = wt.repository();
	if repository.cherry_pick_head().await?.is_none() {
		bail!("there is no cherry-pick to abort (CHERRY_PICK_HEAD is missing)");
	}
	conflict::restore_to_head(wt).await?;
	repository.clear_cherry_pick().await?;
	Ok(())
}

/// Read and parse a commit object.
async fn read_commit<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	oid: ObjectId<H>,
) -> Result<Commit<H>> {
	let (_, payload) = repository.objects().read_object(&oid).await?;
	Ok(parse_commit::<H>(&payload)?)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_object::{ObjectId, Sha256, parse_commit};
	use gitana_repository::{FileMode, Repository, TreeBuildEntry};

	use super::*;
	use crate::test_support::{
		FailingIdentity, TestIdentity, TestSigner, commit_file, fixture, loose_commit,
	};

	/// The author line recorded on `commit`.
	async fn author_of(
		repo: &Repository<impl FileStore, Sha256>,
		commit: ObjectId<Sha256>,
	) -> String {
		let (_, payload) = repo.objects().read_object(&commit).await.unwrap();
		parse_commit::<Sha256>(&payload).unwrap().author
	}

	#[tokio::test]
	async fn picks_a_commit_preserving_the_original_author() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"v1\n", &id).await;
		// An off-branch commit authored by someone else, modifying the same file.
		let repo = wt.repository();
		let blob = repo.write_blob(b"v2\n").await.unwrap();
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "f.txt".to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.unwrap();
		let picker = "Pick Author <pick@example.com> 0 +0000";
		let pick = repo
			.create_commit(tree, vec![a], picker, picker, "pick\n")
			.await
			.unwrap();

		let outcome = cherry_pick(&wt, &pick.to_hex(), &id, None::<&TestSigner>)
			.await
			.unwrap();
		let PickOutcome::Picked { commit } = outcome else {
			panic!("expected a clean pick");
		};
		// The picked author is preserved; the committer is the current identity.
		assert!(author_of(repo, commit).await.contains("Pick Author"));
		let tree = repo.commit_tree(commit).await.unwrap();
		let entries = repo.read_tree(tree).await.unwrap();
		assert_eq!(
			entries,
			vec![("f.txt".to_owned(), "100644".to_owned(), blob)]
		);
	}

	#[tokio::test]
	async fn conflicting_pick_materialises_cherry_pick_head() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		let _ours = commit_file(dir.path(), &wt, "f.txt", b"ours\n", &id).await;
		let pick = loose_commit(wt.repository(), vec![a], "f.txt", b"theirs\n").await;

		let outcome = cherry_pick(&wt, &pick.to_hex(), &id, None::<&TestSigner>)
			.await
			.unwrap();
		let PickOutcome::Conflict { paths } = outcome else {
			panic!("expected a conflict");
		};
		assert_eq!(paths, vec!["f.txt".to_owned()]);
		assert_eq!(
			wt.repository().cherry_pick_head().await.unwrap(),
			Some(pick)
		);
		assert!(wt.load_index().await.unwrap().has_conflicts());
	}

	#[tokio::test]
	async fn conflict_materialises_without_resolving_identity() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		let _ours = commit_file(dir.path(), &wt, "f.txt", b"ours\n", &id).await;
		let pick = loose_commit(wt.repository(), vec![a], "f.txt", b"theirs\n").await;

		// A conflict records CHERRY_PICK_HEAD without recording a commit, so it must not require a
		// configured identity — git materialises the conflict regardless.
		let outcome = cherry_pick(&wt, &pick.to_hex(), &FailingIdentity, None::<&TestSigner>)
			.await
			.unwrap();
		assert!(matches!(outcome, PickOutcome::Conflict { .. }));
		assert_eq!(
			wt.repository().cherry_pick_head().await.unwrap(),
			Some(pick)
		);
	}

	#[tokio::test]
	async fn empty_pick_is_refused() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"v1\n", &id).await;
		// A no-op commit relative to its parent: picking it changes nothing.
		let pick = loose_commit(wt.repository(), vec![a], "f.txt", b"v1\n").await;

		let err = cherry_pick(&wt, &pick.to_hex(), &id, None::<&TestSigner>)
			.await
			.unwrap_err();
		assert!(err.to_string().contains("empty"), "{err}");
	}
}
