use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::commands::conflict::{self, ensure_trailing_newline};
use crate::dispatch::{self, WorkTreeCommand};
use crate::identity;

/// Merge `commit` into the current branch, or carry an in-progress merge to its end.
///
/// Fast-forwards when the current branch is an ancestor of `commit` (unless `--no-ff`), otherwise
/// creates a true two-parent merge commit. `--ff-only` refuses a non-fast-forward. A merge that
/// conflicts materialises an in-progress state (`MERGE_HEAD`, `MERGE_MSG`, a conflicted index, and
/// work-tree markers) and exits non-zero; the user resolves it and then `--continue`s (or
/// `gta commit`s), or `--abort`s to discard it.
pub async fn run(
	cwd: &Path,
	commit: Option<String>,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	abort: bool,
	continue_: bool,
) -> Result<()> {
	if abort && continue_ {
		bail!("--abort and --continue are incompatible");
	}
	dispatch::on_worktree(
		cwd,
		Merge {
			commit,
			message,
			no_ff,
			ff_only,
			abort,
			continue_,
		},
	)
	.await
}

struct Merge {
	commit: Option<String>,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
	abort: bool,
	continue_: bool,
}

impl WorkTreeCommand for Merge {
	async fn run<H: HashAlgorithm>(self, wt: WorkTree<Backend, H>, _prefix: String) -> Result<()> {
		if self.abort {
			return abort_merge(&wt).await;
		}
		if self.continue_ {
			return complete_merge(&wt, None).await;
		}

		let Some(commit) = self.commit else {
			bail!("merge requires a commit (or --abort/--continue)");
		};
		if self.no_ff && self.ff_only {
			bail!("--no-ff and --ff-only are incompatible");
		}
		let repository = wt.repository();

		// Refuse to start a new merge before the previous one is concluded: `MERGE_HEAD` persists until
		// the merge is committed or aborted, even once its conflicts have been resolved and the index is
		// clean again.
		if repository.merge_head().await?.is_some() {
			bail!("you have not concluded your merge (MERGE_HEAD exists)");
		}
		// Symmetrically, refuse to start a merge while a cherry-pick is unconcluded (as git does).
		if repository.cherry_pick_head().await?.is_some() {
			bail!("you have not concluded your cherry-pick (CHERRY_PICK_HEAD exists)");
		}
		// Likewise refuse an index that still carries unmerged stages.
		if wt.load_index()?.has_conflicts() {
			bail!("merging is not possible because you have unmerged files");
		}

		let theirs = repository
			.rev_parse(&format!("{commit}^{{commit}}"))
			.await?;
		// The current tip: the branch's commit, or the detached HEAD object id (git fast-forwards a
		// detached HEAD, and `reset_head` handles both).
		let head_tip = match repository.refs().read_head().await? {
			HeadState::Symbolic(branch) => repository.refs().resolve(&branch).await?,
			HeadState::Detached(id) => Some(id),
		};

		// Already up to date: `commit` is already reachable from the current tip. git reports this even
		// with a dirty work tree, so check it before doing any work.
		if let Some(head) = head_tip
			&& (theirs == head || repository.is_ancestor(theirs, head).await?)
		{
			println!("Already up to date.");
			return Ok(());
		}

		let theirs_tree = repository.commit_tree(theirs).await?;
		let can_fast_forward = match head_tip {
			None => true, // unborn branch
			Some(head) => repository.is_ancestor(head, theirs).await?,
		};

		// Fast-forward (always for an unborn branch — there is no commit to be a merge parent).
		if can_fast_forward && (!self.no_ff || head_tip.is_none()) {
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
			let committer = identity::signature_or_default(repository, "COMMITTER").await;
			repository
				.reset_head(theirs, &committer, &format!("merge {commit}: Fast-forward"))
				.await?;
			match head_tip {
				Some(head) => println!("Updating {}..{}\nFast-forward", short(head), short(theirs)),
				None => println!("Fast-forward"),
			}
			return Ok(());
		}

		if self.ff_only {
			bail!("not possible to fast-forward, aborting");
		}

		// A real merge commit needs the current tip as its first parent.
		let head = head_tip.expect("non-fast-forward merge has a current commit");
		let head_tree = repository.commit_tree(head).await?;

		// A true merge rewrites the whole index from the merged tree, so git refuses any staged change
		// (the index must equal HEAD) — otherwise the materialising checkout would silently drop it.
		let staged = tree_diff_paths(repository, head_tree, conflict::index_tree(&wt).await?).await?;
		if !staged.is_empty() {
			bail!("{}", would_overwrite_message(&staged));
		}

		let message = match self.message {
			Some(message) => message,
			None => default_message(repository, &commit).await,
		};
		let message = ensure_trailing_newline(message);

		// The merged tree: `theirs` itself for a `--no-ff` of a fast-forwardable history, otherwise the
		// three-way merge of the branch and `commit` against their best common ancestor. A conflicting
		// three-way merge materialises an in-progress merge and exits non-zero instead.
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
				return materialise_conflict(
					&wt,
					merge.tree,
					base_tree,
					head_tree,
					theirs_tree,
					head,
					theirs,
					&merge.conflicts,
					&message,
				)
				.await;
			}
			merge.tree
		};

		let author = identity::signature(repository, "AUTHOR").await?;
		let committer = identity::signature(repository, "COMMITTER").await?;

		// Materialise the result first: a checkout that would clobber a touched local change fails here,
		// before any commit is created or the ref is moved.
		wt.checkout(merged_tree, false).await?;
		let merge_commit = repository
			.create_commit(
				merged_tree,
				vec![head, theirs],
				&author,
				&committer,
				&message,
			)
			.await?;
		repository
			.reset_head(
				merge_commit,
				&committer,
				&format!("merge {commit}: Merge made by the 'recursive' strategy."),
			)
			.await?;
		println!("Merge made by the 'recursive' strategy.");
		Ok(())
	}
}

/// Materialise a conflicting merge: write the merged work tree and conflicted index, record
/// `MERGE_HEAD`/`MERGE_MSG`/`ORIG_HEAD`, report the conflicts, and exit non-zero — leaving the state
/// for the user to resolve, as git does.
#[allow(clippy::too_many_arguments)]
async fn materialise_conflict<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	head_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	head: ObjectId<H>,
	theirs: ObjectId<H>,
	conflicts: &[String],
	message: &str,
) -> Result<()> {
	conflict::write_conflicted_state(
		wt,
		merged_tree,
		base_tree,
		head_tree,
		theirs_tree,
		conflicts,
	)
	.await?;
	let repository = wt.repository();
	repository.set_orig_head(head).await?;
	repository.start_merge(theirs, message).await?;
	Err(conflict::report_conflicts(conflicts))
}

/// Conclude an in-progress merge: a two-parent commit from the resolved index. Shared by
/// `merge --continue` (`message_override = None`, uses `MERGE_MSG`) and `gta commit` during a merge
/// (`message_override = Some(..)`). Refuses while the index still has unmerged stages.
pub(crate) async fn complete_merge<H: HashAlgorithm>(
	wt: &WorkTree<Backend, H>,
	message_override: Option<String>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(merge_head) = repository.merge_head().await? else {
		bail!("there is no merge in progress (MERGE_HEAD is missing)");
	};

	let tree = conflict::resolved_tree(wt).await?;
	let author = identity::signature(repository, "AUTHOR").await?;
	let committer = identity::signature(repository, "COMMITTER").await?;
	let message = match message_override {
		Some(message) => message,
		None => repository
			.merge_msg()
			.await?
			.unwrap_or_else(|| format!("Merge commit '{merge_head}'")),
	};
	let message = ensure_trailing_newline(message);

	let commit = repository
		.commit_merge(tree, merge_head, &author, &committer, &message)
		.await?;
	repository.clear_merge().await?;
	println!("{commit}");
	Ok(())
}

/// Abort an in-progress merge: restore the work tree and index to the (unmoved) `HEAD`, discarding
/// conflict markers and unmerged stages, and clear the merge state. Like `git merge --abort`.
async fn abort_merge<H: HashAlgorithm>(wt: &WorkTree<Backend, H>) -> Result<()> {
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
async fn tree_diff_paths<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
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
async fn default_message<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
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
async fn virtual_base_tree<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
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

fn short<H: HashAlgorithm>(id: ObjectId<H>) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}
