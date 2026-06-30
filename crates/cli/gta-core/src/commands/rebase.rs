//! `gta rebase` — replay the current branch's commits onto a new base.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{Commit, HashAlgorithm, ObjectId, parse_commit};
use gitana_repository::{HeadState, RebaseState, Repository};
use gitana_worktree::WorkTree;

use crate::Backend;
use crate::commands::conflict::{self, ensure_trailing_newline};
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

/// Rebase the current branch onto `upstream` (or `--onto <newbase>`), or carry an in-progress rebase
/// to its end.
///
/// Replays the branch's commits that are not in `upstream`, oldest-first, as fresh cherry-picks on
/// the new base. A conflict stops the rebase with a materialised conflict (the `REBASE_*` state, a
/// conflicted index, work-tree markers); resolve it and `--continue`, drop the commit with `--skip`,
/// or restore the original branch with `--abort`. Linear histories only (a merge commit in the range
/// is refused); commits that become empty are dropped, while originally-empty commits are kept.
pub async fn run(
	cwd: &Path,
	upstream: Option<String>,
	onto: Option<String>,
	abort: bool,
	continue_: bool,
	skip: bool,
) -> Result<()> {
	if [abort, continue_, skip].iter().filter(|&&f| f).count() > 1 {
		bail!("--abort, --continue, and --skip are mutually exclusive");
	}
	dispatch::on_worktree(
		cwd,
		Rebase {
			upstream,
			onto,
			abort,
			continue_,
			skip,
		},
	)
	.await
}

struct Rebase {
	upstream: Option<String>,
	onto: Option<String>,
	abort: bool,
	continue_: bool,
	skip: bool,
}

impl WorkTreeCommand for Rebase {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return abort_rebase(&wt).await;
		}
		if self.continue_ {
			return continue_rebase(&wt).await;
		}
		if self.skip {
			return skip_rebase(&wt).await;
		}
		start(&wt, self.upstream, self.onto).await
	}
}

/// Begin a rebase: validate, compute the commits to replay, move the branch to the base, and replay.
async fn start<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
	upstream: Option<String>,
	onto: Option<String>,
) -> Result<()> {
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
	// git requires a clean work tree to rebase: no staged or unstaged tracked changes (untracked
	// files are allowed, unless the checkout to the base would overwrite one — caught below).
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

	let committer = identity::signature_or_default(repository, "COMMITTER").await;

	// Nothing to replay: the branch is already on (or behind) the base — a fast-forward or no-op.
	if todo.is_empty() {
		if onto == head_tip {
			println!("Current branch {} is up to date.", branch_short(&head_name));
		} else {
			move_branch_to(wt, onto, &committer, "rebase: fast-forward").await?;
			println!(
				"Fast-forwarded {} to {}.",
				branch_short(&head_name),
				short(onto)
			);
		}
		return Ok(());
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
	replay(wt).await
}

/// Replay the remaining commits from the persisted state until one conflicts or the list is empty.
async fn replay<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		return Ok(());
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
				let committer = identity::signature(repository, "COMMITTER").await?;
				let message = ensure_trailing_newline(commit.message.clone());
				repository
					.commit_on_head(head_tree, &commit.author, &committer, &message)
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
			// `todo` still has `current` at its front (persisted), so `--continue` resumes here.
			for path in &merge.conflicts {
				println!("CONFLICT (content): Merge conflict in {path}");
			}
			println!("could not apply {} {}", short(current), subject(&commit));
			println!(
				"hint: resolve the conflicts, `gta add` them, then run `gta rebase --continue` (or --skip / --abort)"
			);
			return Err(crate::MergeConflict.into());
		}

		wt.checkout(merge.tree, false).await?;
		let committer = identity::signature(repository, "COMMITTER").await?;
		let message = ensure_trailing_newline(commit.message.clone());
		repository
			.commit_on_head(merge.tree, &commit.author, &committer, &message)
			.await?;
		todo.remove(0);
		repository.set_rebase_todo(&todo).await?;
	}

	repository.clear_rebase().await?;
	println!(
		"Successfully rebased and updated {}.",
		branch_short(&state.head_name)
	);
	Ok(())
}

/// Conclude the stopped step (commit the resolved index, preserving the commit's author), then resume.
async fn continue_rebase<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress");
	};
	let Some(&current) = state.todo.first() else {
		return replay(wt).await; // nothing pending; let replay finish/clean up
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
	let committer = identity::signature(repository, "COMMITTER").await?;
	let message = ensure_trailing_newline(commit.message.clone());
	repository
		.commit_on_head(tree, &commit.author, &committer, &message)
		.await?;
	advance_todo(repository, state.todo).await?;
	replay(wt).await
}

/// Drop the stopped commit and resume.
async fn skip_rebase<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress");
	};
	if state.todo.is_empty() {
		return replay(wt).await;
	}
	conflict::restore_to_head(wt).await?; // discard the conflicted work tree / index
	advance_todo(repository, state.todo).await?;
	replay(wt).await
}

/// Abort the rebase: restore the branch and work tree to the pre-rebase tip.
async fn abort_rebase<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
	let repository = wt.repository();
	let Some(state) = repository.rebase_state().await? else {
		bail!("no rebase in progress (no rebase-merge state)");
	};
	let committer = identity::signature_or_default(repository, "COMMITTER").await;
	repository
		.reset_head(state.orig_head, &committer, "rebase: aborting")
		.await?;
	let orig_tree = repository.commit_tree(state.orig_head).await?;
	wt.checkout(orig_tree, true).await?;
	repository.clear_rebase().await?;
	Ok(())
}

/// Move the current branch to `target` and update the work tree to match.
async fn move_branch_to<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
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
async fn advance_todo<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
	mut todo: Vec<ObjectId<H>>,
) -> Result<()> {
	todo.remove(0);
	repository.set_rebase_todo(&todo).await?;
	Ok(())
}

/// The commits reachable from `head` but not from `base` (the merge base), oldest-first.
async fn commits_to_replay<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
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

async fn branch_tip<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
	branch: &str,
) -> Result<ObjectId<H>> {
	repository
		.refs()
		.resolve(branch)
		.await?
		.ok_or_else(|| anyhow::anyhow!("rebase branch {branch} has no tip"))
}

/// Read and parse a commit object.
async fn read_commit<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
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

fn branch_short(branch: &str) -> &str {
	branch.strip_prefix("refs/heads/").unwrap_or(branch)
}
