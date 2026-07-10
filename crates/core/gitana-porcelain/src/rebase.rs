//! `rebase` — replay the current branch's commits onto a new base.

use std::collections::HashSet;

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, RebaseState, Repository};
use gitana_worktree::WorkTree;

use crate::conflict;
use crate::{Identity, Signer, signing};

/// The result of starting a [`rebase`] or resuming one ([`continue_rebase`] / [`skip_rebase`]). In
/// each variant `branch` is the full ref name (`refs/heads/<name>`); the adapter shortens it.
#[derive(Debug)]
pub enum RebaseOutcome<H: HashAlgorithm> {
	/// The branch is already on the base; nothing to replay.
	UpToDate { branch: String },
	/// The branch had nothing to replay but was behind the base, so it was fast-forwarded to `onto`.
	FastForwarded { branch: String, onto: ObjectId<H> },
	/// Every commit was replayed; the rebase is complete.
	Rebased { branch: String },
	/// A commit could not be applied cleanly; the rebase stopped with a materialised conflict (the
	/// `REBASE_*` state, a conflicted index and work tree). Resolve and `continue`, `skip`, or `abort`.
	Conflict {
		commit: ObjectId<H>,
		subject: String,
		paths: Vec<String>,
	},
}

/// Begin a rebase of the current branch onto `upstream` (or `--onto <newbase>`): validate, compute the
/// commits to replay, move the branch to the base, and replay them.
///
/// Replays the branch's commits that are not in `upstream`, oldest-first, as fresh cherry-picks. A
/// merge commit in the range is refused (linear histories only); commits that become empty are
/// dropped, while originally-empty commits are kept.
pub async fn rebase<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	upstream: Option<String>,
	onto: Option<String>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<RebaseOutcome<H>> {
	let repository = wt.repository();
	if let Some(op) = conflict::operation_in_progress(repository).await? {
		bail!("a {op} is already in progress; conclude it (`--continue`) or abort it (`--abort`)");
	}
	let Some(upstream) = upstream else {
		bail!("rebase requires an <upstream> (or --abort/--continue/--skip)");
	};

	// The branch being rebased must be a real branch (detached HEAD is not supported here).
	let head_name = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot rebase a detached HEAD (not yet supported)"),
	};
	let Some(head_tip) = repository.refs().resolve(&head_name).await? else {
		bail!("cannot rebase an unborn branch");
	};
	// git requires a clean work tree to rebase: no staged or unstaged tracked changes (untracked files
	// are allowed, unless the checkout to the base would overwrite one — caught below).
	if !wt.status().await?.changed.is_empty() {
		bail!("cannot rebase: you have uncommitted changes; commit or stash them before rebasing");
	}

	let upstream_tip = repository
		.rev_parse(&format!("{upstream}^{{commit}}"))
		.await?;
	let onto = match &onto {
		Some(spec) => repository.rev_parse(&format!("{spec}^{{commit}}")).await?,
		None => upstream_tip,
	};

	// Commits to replay: reachable from HEAD but not from the merge base, oldest-first.
	let base = repository
		.merge_base(&[head_tip, upstream_tip])
		.await?
		.into_iter()
		.next();
	let todo = commits_to_replay(repository, head_tip, base).await?;
	for &oid in &todo {
		if read_commit(repository, oid).await?.parents.len() > 1 {
			bail!(
				"cannot rebase: {} is a merge commit (--rebase-merges is not supported)",
				short(oid)
			);
		}
	}

	let committer = identity.committer_or_default().await?;

	// Nothing to replay: the branch is already on (or behind) the base — a fast-forward or no-op.
	if todo.is_empty() {
		if onto == head_tip {
			return Ok(RebaseOutcome::UpToDate { branch: head_name });
		}
		move_branch_to(wt, onto, &committer, "rebase: fast-forward").await?;
		return Ok(RebaseOutcome::FastForwarded {
			branch: head_name,
			onto,
		});
	}

	repository
		.start_rebase(&RebaseState {
			head_name: head_name.clone(),
			orig_head: head_tip,
			onto,
			todo,
		})
		.await?;
	// Move to the base; if that checkout fails (e.g. an untracked file is in the way), roll back the
	// state just written so a failed start does not leave a phantom rebase in progress, as git does.
	if let Err(error) = move_branch_to(
		wt,
		onto,
		&committer,
		&format!("rebase: checkout {}", short(onto)),
	)
	.await
	{
		repository.clear_rebase().await.ok();
		return Err(error);
	}
	replay(wt, identity, signer).await
}

/// Conclude the stopped step (commit the resolved index, preserving the commit's author), then resume.
pub async fn continue_rebase<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<RebaseOutcome<H>> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress");
	};
	let Some(&current) = state.todo.first() else {
		return replay(wt, identity, signer).await; // nothing pending; let replay finish/clean up
	};

	let tree = conflict::resolved_tree(wt).await?; // refuses while the index has conflicts
	let head_tree = repository
		.commit_tree(branch_tip(repository, &state.head_name).await?)
		.await?;
	if tree == head_tree {
		bail!(
			"no changes - did you forget to `gta add`, or do you want `gta rebase --skip` to drop this commit?"
		);
	}
	let commit = read_commit(repository, current).await?;
	let committer = identity.committer().await?;
	let message = conflict::ensure_trailing_newline(commit.message.clone());
	signing::commit_on_head(
		repository,
		tree,
		&commit.author,
		&committer,
		&message,
		signer,
	)
	.await?;
	advance_todo(repository, state.todo).await?;
	replay(wt, identity, signer).await
}

/// Drop the stopped commit and resume.
pub async fn skip_rebase<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<RebaseOutcome<H>> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress");
	};
	if state.todo.is_empty() {
		return replay(wt, identity, signer).await;
	}
	conflict::restore_to_head(wt).await?; // discard the conflicted work tree / index
	advance_todo(repository, state.todo).await?;
	replay(wt, identity, signer).await
}

/// Abort the rebase: restore the branch and work tree to the pre-rebase tip.
pub async fn abort_rebase<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	identity: &impl Identity,
) -> Result<()> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress (no rebase-merge state)");
	};
	let committer = identity.committer_or_default().await?;
	repository
		.reset_head(state.orig_head, &committer, "rebase: aborting")
		.await?;
	let orig_tree = repository.commit_tree(state.orig_head).await?;
	wt.checkout(orig_tree, true).await?;
	repository.clear_rebase().await?;
	Ok(())
}

/// Replay the remaining commits from the persisted state until one conflicts or the list is empty.
async fn replay<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<RebaseOutcome<H>> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress");
	};
	let mut todo = state.todo;

	while let Some(&current) = todo.first() {
		let commit = read_commit(repository, current).await?;
		let base_tree = match commit.parents.first() {
			Some(parent) => repository.commit_tree(*parent).await?,
			None => repository.write_tree(&[]).await?,
		};
		let head_tip = branch_tip(repository, &state.head_name).await?;
		let head_tree = repository.commit_tree(head_tip).await?;
		let merge = repository
			.merge_trees(base_tree, head_tree, commit.tree)
			.await?;

		// No net change after replay. git's default keeps a commit that was empty in the original
		// history (re-create it empty) but drops one that became empty because its change is already
		// present in the new base.
		if merge.tree == head_tree {
			if base_tree == commit.tree {
				let committer = identity.committer().await?;
				let message = conflict::ensure_trailing_newline(commit.message.clone());
				signing::commit_on_head(
					repository,
					head_tree,
					&commit.author,
					&committer,
					&message,
					signer,
				)
				.await?;
			}
			todo.remove(0);
			repository.set_rebase_todo(&todo).await?;
			continue;
		}

		if !merge.conflicts.is_empty() {
			conflict::write_conflicted_state(
				wt,
				merge.tree,
				base_tree,
				head_tree,
				commit.tree,
				&merge.conflicts,
			)
			.await?;
			// `todo` still has `current` at its front (persisted), so `--continue` resumes here. No
			// identity is resolved — the conflict only materialises state, no commit is recorded.
			return Ok(RebaseOutcome::Conflict {
				commit: current,
				subject: subject(&commit).to_owned(),
				paths: merge.conflicts,
			});
		}

		wt.checkout(merge.tree, false).await?;
		let committer = identity.committer().await?;
		let message = conflict::ensure_trailing_newline(commit.message.clone());
		signing::commit_on_head(
			repository,
			merge.tree,
			&commit.author,
			&committer,
			&message,
			signer,
		)
		.await?;
		todo.remove(0);
		repository.set_rebase_todo(&todo).await?;
	}

	repository.clear_rebase().await?;
	Ok(RebaseOutcome::Rebased {
		branch: state.head_name,
	})
}

/// Move the current branch to `target` and update the work tree to match.
async fn move_branch_to<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	target: ObjectId<H>,
	committer: &str,
	reflog: &str,
) -> Result<()> {
	let repository = wt.repository();
	let tree = repository.commit_tree(target).await?;
	wt.checkout(tree, false).await?;
	repository.reset_head(target, committer, reflog).await?;
	Ok(())
}

/// Drop the first (current) entry from `todo` and persist the remainder.
async fn advance_todo<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	mut todo: Vec<ObjectId<H>>,
) -> Result<()> {
	todo.remove(0);
	repository.set_rebase_todo(&todo).await?;
	Ok(())
}

/// The commits reachable from `head` but not from `base` (the merge base), oldest-first.
async fn commits_to_replay<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	head: ObjectId<H>,
	base: Option<ObjectId<H>>,
) -> Result<Vec<ObjectId<H>>> {
	let base_ancestors: HashSet<ObjectId<H>> = match base {
		Some(base) => repository.rev_list(&[base]).await?.into_iter().collect(),
		None => HashSet::new(),
	};
	let mut commits: Vec<ObjectId<H>> = repository
		.rev_list(&[head])
		.await?
		.into_iter()
		.filter(|oid| !base_ancestors.contains(oid))
		.collect();
	commits.reverse(); // rev_list is newest-first; replay oldest-first
	Ok(commits)
}

async fn branch_tip<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	branch: &str,
) -> Result<ObjectId<H>> {
	repository
		.refs()
		.resolve(branch)
		.await?
		.ok_or_else(|| anyhow::anyhow!("rebase branch {branch} has no tip"))
}

/// Read and parse a commit object.
async fn read_commit<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	oid: ObjectId<H>,
) -> Result<Commit<H>> {
	let (_, payload) = repository.objects().read_object(&oid).await?;
	Ok(parse_commit::<H>(&payload)?)
}

fn subject<H: HashAlgorithm>(commit: &Commit<H>) -> &str {
	commit.message.lines().next().unwrap_or("")
}

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_object::{ObjectId, Sha256};

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, commit_file, fixture, loose_commit};

	/// Point `refs/heads/upstream` at `tip`.
	async fn set_upstream(
		wt: &WorkTree<impl FileStore, impl WorkDirFs, Sha256>,
		tip: ObjectId<Sha256>,
	) {
		wt.repository()
			.refs()
			.update_ref(
				"refs/heads/upstream",
				tip,
				None,
				gitana_repository::ReflogIntent::Skip,
			)
			.await
			.unwrap();
	}

	#[tokio::test]
	async fn up_to_date_when_already_on_the_base() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		set_upstream(&wt, a).await; // upstream == the branch tip

		let outcome = rebase(
			&wt,
			Some("upstream".to_owned()),
			None,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		assert!(matches!(outcome, RebaseOutcome::UpToDate { .. }));
	}

	#[tokio::test]
	async fn fast_forwards_when_the_branch_is_behind() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		// upstream is a descendant of the branch tip, and the branch has nothing of its own to replay.
		let u = loose_commit(wt.repository(), vec![a], "u.txt", b"u\n").await;
		set_upstream(&wt, u).await;

		let outcome = rebase(
			&wt,
			Some("upstream".to_owned()),
			None,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		assert!(matches!(outcome, RebaseOutcome::FastForwarded { onto, .. } if onto == u));
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(u)
		);
	}

	#[tokio::test]
	async fn linear_rebase_replays_onto_upstream() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		let u = loose_commit(wt.repository(), vec![a], "u.txt", b"u\n").await;
		set_upstream(&wt, u).await;
		// A commit of the branch's own, diverging from `a` on a different path (clean to replay).
		let b = commit_file(dir.path(), &wt, "g.txt", b"b\n", &id).await;

		let outcome = rebase(
			&wt,
			Some("upstream".to_owned()),
			None,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		assert!(matches!(outcome, RebaseOutcome::Rebased { .. }));
		let repo = wt.repository();
		let tip = repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.unwrap()
			.unwrap();
		// The branch was rewritten onto upstream: a new tip (not `b`) with `u` as an ancestor.
		assert_ne!(tip, b);
		assert!(repo.is_ancestor(u, tip).await.unwrap());
	}

	#[tokio::test]
	async fn a_conflict_stops_the_rebase() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"base\n", &id).await;
		// upstream and the branch both change the same file from the base → replay conflicts.
		let u = loose_commit(wt.repository(), vec![a], "f.txt", b"upstream\n").await;
		set_upstream(&wt, u).await;
		let b = commit_file(dir.path(), &wt, "f.txt", b"mine\n", &id).await;

		let outcome = rebase(
			&wt,
			Some("upstream".to_owned()),
			None,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();
		let RebaseOutcome::Conflict {
			commit,
			subject,
			paths,
		} = outcome
		else {
			panic!("expected a conflict");
		};
		assert_eq!(commit, b);
		assert_eq!(subject, "add f.txt");
		assert_eq!(paths, vec!["f.txt".to_owned()]);
		assert!(wt.repository().rebase_in_progress().await.unwrap());
	}
}
