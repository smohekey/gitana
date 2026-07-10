//! `merge` — merge a commit into the current branch (fast-forward or a true two-parent merge),
//! conclude an in-progress merge, or abort one. The start path returns a [`MergeOutcome`] that the
//! CLI adapter renders; a conflict materialises in-progress state and reports its paths as data
//! rather than printing.

use anyhow::{Context, Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::conflict;
use crate::{Identity, Signer, signing};

/// The result of starting a [`merge`].
#[derive(Debug)]
pub enum MergeOutcome<H: HashAlgorithm> {
	/// `commit` is already reachable from the current tip; nothing was done.
	AlreadyUpToDate,
	/// The branch was fast-forwarded to `to`. `from` is the previous tip, or `None` for an unborn
	/// branch (there is no `Updating a..b` range to show).
	FastForward {
		from: Option<ObjectId<H>>,
		to: ObjectId<H>,
	},
	/// A true two-parent merge commit was recorded on the branch.
	Made { commit: ObjectId<H> },
	/// The merge conflicted; an in-progress merge has been materialised (`MERGE_HEAD`, `MERGE_MSG`, a
	/// conflicted index and work tree). The caller renders the conflicted paths and signals failure.
	Conflict { paths: Vec<String> },
}

/// Merge `commit_spec` into the current branch.
///
/// Fast-forwards when the current tip is an ancestor of `commit_spec` (unless `no_ff`), otherwise
/// records a true two-parent merge; `ff_only` refuses a non-fast-forward. Identity is resolved only
/// once a commit will actually be made.
pub async fn merge<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	commit_spec: &str,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<MergeOutcome<H>> {
	if no_ff && ff_only {
		bail!("--no-ff and --ff-only are incompatible");
	}
	let repository = wt.repository();

	// Refuse to start while another history-editing operation is unconcluded, or the index still
	// carries unmerged stages.
	if let Some(op) = conflict::operation_in_progress(repository).await? {
		bail!("a {op} is already in progress; conclude it (`--continue`) or abort it (`--abort`)");
	}
	if wt.load_index().await?.has_conflicts() {
		bail!("merging is not possible because you have unmerged files");
	}

	let theirs = repository
		.rev_parse(&format!("{commit_spec}^{{commit}}"))
		.await?;
	// The current tip: the branch's commit, or the detached HEAD object id (git fast-forwards a
	// detached HEAD, and `reset_head` handles both).
	let head_tip = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => repository.refs().resolve(&branch).await?,
		HeadState::Detached(id) => Some(id),
	};

	// Already up to date: `commit_spec` is already reachable from the current tip. git reports this
	// even with a dirty work tree, so check it before doing any work.
	if let Some(head) = head_tip
		&& (theirs == head || repository.is_ancestor(theirs, head).await?)
	{
		return Ok(MergeOutcome::AlreadyUpToDate);
	}

	let theirs_tree = repository.commit_tree(theirs).await?;
	let can_fast_forward = match head_tip {
		None => true, // unborn branch
		Some(head) => repository.is_ancestor(head, theirs).await?,
	};

	// Fast-forward (always for an unborn branch — there is no commit to be a merge parent).
	if can_fast_forward && (!no_ff || head_tip.is_none()) {
		// Apply only the HEAD→theirs diff (git's two-way merge), so unrelated staged or dirty files
		// survive; a local change to a path the fast-forward updates is refused, not clobbered.
		let from_tree = match head_tip {
			Some(head) => repository.commit_tree(head).await?,
			None => repository.write_tree(&[]).await?,
		};
		let overwrite = wt.twoway_merge(from_tree, theirs_tree).await?;
		if !overwrite.is_empty() {
			bail!("{}", would_overwrite_message(&overwrite));
		}
		let committer = identity.committer_or_default().await?;
		repository
			.reset_head(
				theirs,
				&committer,
				&format!("merge {commit_spec}: Fast-forward"),
			)
			.await?;
		return Ok(MergeOutcome::FastForward {
			from: head_tip,
			to: theirs,
		});
	}

	if ff_only {
		bail!("not possible to fast-forward, aborting");
	}

	// A real merge commit needs the current tip as its first parent.
	let head = head_tip.expect("non-fast-forward merge has a current commit");
	let head_tree = repository.commit_tree(head).await?;

	// A true merge rewrites the whole index from the merged tree, so git refuses any staged change
	// (the index must equal HEAD) — otherwise the materialising checkout would silently drop it.
	let staged = tree_diff_paths(repository, head_tree, conflict::index_tree(wt).await?).await?;
	if !staged.is_empty() {
		bail!("{}", would_overwrite_message(&staged));
	}

	let message = match message {
		Some(message) => message,
		None => default_message(repository, commit_spec).await,
	};
	let message = conflict::ensure_trailing_newline(message);

	// The merged tree: `theirs` itself for a `--no-ff` of a fast-forwardable history, otherwise the
	// three-way merge of the branch and `commit` against their best common ancestor. A conflicting
	// three-way merge materialises an in-progress merge and returns its paths instead.
	let merged_tree = if can_fast_forward {
		theirs_tree
	} else {
		let bases = repository.merge_base(&[head, theirs]).await?;
		if bases.is_empty() {
			bail!("refusing to merge unrelated histories");
		}
		// Reduce multiple merge bases (a criss-cross history) to one virtual base tree, as git's
		// recursive strategy does — otherwise such merges report false conflicts.
		let base_tree = virtual_base_tree(repository, &bases).await?;
		let merge = repository
			.merge_trees(base_tree, head_tree, theirs_tree)
			.await?;
		if !merge.conflicts.is_empty() {
			// Materialise the conflict for the user to resolve: conflicted work tree and index,
			// `ORIG_HEAD`, then `MERGE_HEAD`/`MERGE_MSG`.
			conflict::write_conflicted_state(
				wt,
				merge.tree,
				base_tree,
				head_tree,
				theirs_tree,
				&merge.conflicts,
			)
			.await?;
			repository.set_orig_head(head).await?;
			repository.start_merge(theirs, &message).await?;
			return Ok(MergeOutcome::Conflict {
				paths: merge.conflicts,
			});
		}
		merge.tree
	};

	let author = identity.author().await?;
	let committer = identity.committer().await?;

	// Build (and, when configured, sign) the merge commit *before* touching the work tree: signing can
	// fail (bad `gpg.format`, missing key, `ssh-keygen` error), and writing the object has no
	// observable effect until a ref points at it — so a failure here leaves the work tree untouched
	// rather than materialised-but-uncommitted. Then materialise the result (a checkout that would
	// clobber a touched local change fails here, before the ref moves) and advance the branch.
	let merge_commit = signing::seal_commit(
		repository,
		merged_tree,
		vec![head, theirs],
		&author,
		&committer,
		&message,
		signer,
	)
	.await?;
	wt.checkout(merged_tree, false).await?;
	repository
		.reset_head(
			merge_commit,
			&committer,
			&format!("merge {commit_spec}: Merge made by the 'recursive' strategy."),
		)
		.await?;
	Ok(MergeOutcome::Made {
		commit: merge_commit,
	})
}

/// Conclude an in-progress merge: a two-parent commit from the resolved index, returning the new
/// commit id. Shared by `merge --continue` (`message_override = None`, uses `MERGE_MSG`) and
/// `gta commit` during a merge (`message_override = Some(..)`). Refuses while the index still has
/// unmerged stages — checked before identity is resolved.
pub async fn continue_merge<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	message_override: Option<String>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<ObjectId<H>> {
	let repository = wt.repository();
	let Some(merge_head) = repository.merge_head().await? else {
		bail!("there is no merge in progress (MERGE_HEAD is missing)");
	};

	let tree = conflict::resolved_tree(wt).await?;
	let author = identity.author().await?;
	let committer = identity.committer().await?;
	let message = match message_override {
		Some(message) => message,
		None => repository
			.merge_msg()
			.await?
			.unwrap_or_else(|| format!("Merge commit '{merge_head}'")),
	};
	let message = conflict::ensure_trailing_newline(message);

	// Build the (optionally signed) two-parent commit, then move the ref with the `commit (merge):`
	// reflog. A merge in progress implies the branch has a tip.
	let (target, parent) = repository.head_branch_tip().await?;
	let parent = parent.context("a merge is in progress but the branch is unborn")?;
	let commit = signing::seal_commit(
		repository,
		tree,
		vec![parent, merge_head],
		&author,
		&committer,
		&message,
		signer,
	)
	.await?;
	repository
		.record_merge_commit(&target, parent, commit, &committer, &message)
		.await?;
	repository.clear_merge().await?;
	Ok(commit)
}

/// Abort an in-progress merge: restore the work tree and index to the (unmoved) `HEAD`, discarding
/// conflict markers and unmerged stages, and clear the merge state. Like `git merge --abort`.
pub async fn abort_merge<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<()> {
	let repository = wt.repository();
	if repository.merge_head().await?.is_none() {
		bail!("there is no merge to abort (MERGE_HEAD is missing)");
	}
	// HEAD does not move while a merge is in progress, so restoring it is the pre-merge state.
	conflict::restore_to_head(wt).await?;
	repository.clear_merge().await?;
	Ok(())
}

/// git's "your local changes would be overwritten" refusal, listing the offending paths.
fn would_overwrite_message(paths: &[String]) -> String {
	format!(
		"Your local changes to the following files would be overwritten by merge:\n  {}\nPlease commit your changes or stash them before you merge.",
		paths.join("\n  ")
	)
}

/// The paths that differ between two trees (added/removed/modified), sorted.
async fn tree_diff_paths<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	a: ObjectId<H>,
	b: ObjectId<H>,
) -> Result<Vec<String>> {
	use std::collections::{HashMap, HashSet};
	let map = |entries: Vec<(String, String, ObjectId<H>)>| {
		entries
			.into_iter()
			.map(|(path, mode, oid)| (path, (mode, oid)))
			.collect::<HashMap<_, _>>()
	};
	let am = map(repository.read_tree(a).await?);
	let bm = map(repository.read_tree(b).await?);
	let mut paths: Vec<String> = am
		.keys()
		.chain(bm.keys())
		.cloned()
		.collect::<HashSet<_>>()
		.into_iter()
		.filter(|path| am.get(path) != bm.get(path))
		.collect();
	paths.sort();
	Ok(paths)
}

/// git's default merge message: `Merge branch '<name>'` when the argument names a local branch,
/// otherwise `Merge commit '<arg>'`.
async fn default_message<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	arg: &str,
) -> String {
	let is_branch = repository
		.refs()
		.resolve(&format!("refs/heads/{arg}"))
		.await
		.ok()
		.flatten()
		.is_some();
	if is_branch {
		format!("Merge branch '{arg}'")
	} else {
		format!("Merge commit '{arg}'")
	}
}

/// Reduce the merge bases to a single base tree for the three-way merge. With one base it is that
/// commit's tree; with several (a criss-cross history) the base trees are folded together, each
/// against its own common ancestor, into a "virtual" base — the way git's recursive strategy does.
/// (Deeply nested criss-crosses are approximated: the virtual base is not re-inserted into the
/// commit graph for later merge-base queries.)
async fn virtual_base_tree<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	bases: &[ObjectId<H>],
) -> Result<ObjectId<H>> {
	let (first, rest) = bases.split_first().expect("at least one merge base");
	let mut base_tree = repository.commit_tree(*first).await?;
	let mut base_commit = *first;
	for &next in rest {
		let sub_bases = repository.merge_base(&[base_commit, next]).await?;
		let sub_base_tree = match sub_bases.first() {
			Some(&sub) => repository.commit_tree(sub).await?,
			None => repository.write_tree(&[]).await?, // unrelated base commits: empty base
		};
		let next_tree = repository.commit_tree(next).await?;
		base_tree = repository
			.merge_trees(sub_base_tree, base_tree, next_tree)
			.await?
			.tree;
		base_commit = next;
	}
	Ok(base_tree)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use super::*;
	use crate::test_support::{
		FailingSigner, TestIdentity, TestSigner, commit_file, fixture, loose_commit,
	};

	#[tokio::test]
	async fn a_failed_signature_leaves_a_clean_merge_recoverable() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		let ours = commit_file(dir.path(), &wt, "ours.txt", b"ours\n", &id).await;
		let theirs = loose_commit(wt.repository(), vec![a], "theirs.txt", b"theirs\n").await;

		// A true merge that would succeed, but signing fails: the object write happens before the
		// checkout, so the branch stays at `ours` and the work tree/index are untouched — not left
		// materialised-but-uncommitted with no way to continue or abort.
		let err = merge(
			&wt,
			&theirs.to_hex(),
			None,
			false,
			false,
			&id,
			Some(&FailingSigner),
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("signing failed"), "{err}");
		let repo = wt.repository();
		assert_eq!(
			repo.refs().resolve("refs/heads/main").await.unwrap(),
			Some(ours),
			"the branch must not move on a failed signed merge"
		);
		assert_eq!(repo.merge_head().await.unwrap(), None);
		// The index still matches HEAD (the clean pre-merge state), so the tree was not materialised.
		let head_tree = repo.commit_tree(ours).await.unwrap();
		assert_eq!(conflict::index_tree(&wt).await.unwrap(), head_tree);
	}

	#[tokio::test]
	async fn already_up_to_date_when_target_is_reachable() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;

		let outcome = merge(
			&wt,
			&a.to_hex(),
			None,
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		assert!(matches!(outcome, MergeOutcome::AlreadyUpToDate));
	}

	#[tokio::test]
	async fn fast_forward_advances_the_branch() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		// A descendant of the current tip, off-branch: merging it fast-forwards.
		let b = loose_commit(wt.repository(), vec![a], "f.txt", b"b\n").await;

		let outcome = merge(
			&wt,
			&b.to_hex(),
			None,
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		assert!(
			matches!(outcome, MergeOutcome::FastForward { from, to } if from == Some(a) && to == b)
		);
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(b)
		);
	}

	#[tokio::test]
	async fn true_merge_makes_a_two_parent_commit() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		// `ours` advances main on one path; `theirs` diverges from `a` on another → a clean true merge.
		let ours = commit_file(dir.path(), &wt, "ours.txt", b"ours\n", &id).await;
		let theirs = loose_commit(wt.repository(), vec![a], "theirs.txt", b"theirs\n").await;

		let outcome = merge(
			&wt,
			&theirs.to_hex(),
			None,
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		let MergeOutcome::Made { commit } = outcome else {
			panic!("expected a merge commit");
		};
		// Both tips are ancestors of the merge commit — i.e. it has them as its two parents.
		let repo = wt.repository();
		assert!(repo.is_ancestor(ours, commit).await.unwrap());
		assert!(repo.is_ancestor(theirs, commit).await.unwrap());
		assert_eq!(
			repo.refs().resolve("refs/heads/main").await.unwrap(),
			Some(commit)
		);
	}

	#[tokio::test]
	async fn conflict_materialises_merge_head_and_returns_paths() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		// Both sides change the same path from the base → a content conflict.
		let _ours = commit_file(dir.path(), &wt, "f.txt", b"ours\n", &id).await;
		let theirs = loose_commit(wt.repository(), vec![a], "f.txt", b"theirs\n").await;

		let outcome = merge(
			&wt,
			&theirs.to_hex(),
			None,
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		let MergeOutcome::Conflict { paths } = outcome else {
			panic!("expected a conflict");
		};
		assert_eq!(paths, vec!["f.txt".to_owned()]);
		// The in-progress merge is recorded for the user to resolve.
		assert_eq!(wt.repository().merge_head().await.unwrap(), Some(theirs));
		assert!(wt.load_index().await.unwrap().has_conflicts());
	}
}
