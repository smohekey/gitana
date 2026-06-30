use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::commands::commit::index_tree_entries;
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
	async fn run<H: HashAlgorithm>(
		self,
		wt: WorkTree<LocalFileStore, H>,
		_prefix: String,
	) -> Result<()> {
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
		// clean again. (cherry-pick/revert state files would be added with those commands.)
		if repository.merge_head().await?.is_some() {
			bail!("you have not concluded your merge (MERGE_HEAD exists)");
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
			wt.checkout(theirs_tree, false).await?;
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
			let head_tree = repository.commit_tree(head).await?;
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

/// Materialise a conflicting merge: write the merged work tree (conflict files carry markers) and a
/// conflicted index (stages 1/2/3 from base/ours/theirs), record `MERGE_HEAD`/`MERGE_MSG`/`ORIG_HEAD`,
/// report the conflicts, and exit non-zero — leaving the state for the user to resolve, as git does.
#[allow(clippy::too_many_arguments)]
async fn materialise_conflict<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	head_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	head: ObjectId<H>,
	theirs: ObjectId<H>,
	conflicts: &[String],
	message: &str,
) -> Result<()> {
	let repository = wt.repository();

	// Write the merged result (markers in conflicted files) and a stage-0 index, refusing — before
	// any state is recorded — if it would clobber a touched local change.
	wt.checkout(merged_tree, false).await?;

	let base = tree_entry_map(repository, base_tree).await?;
	let ours = tree_entry_map(repository, head_tree).await?;
	let theirs_entries = tree_entry_map(repository, theirs_tree).await?;
	let mut index = wt.load_index()?;
	for path in conflicts {
		index.record_conflict(
			path,
			base.get(path).copied(),
			ours.get(path).copied(),
			theirs_entries.get(path).copied(),
		);
	}
	wt.save_index(&index)?;

	repository.set_orig_head(head).await?;
	repository.start_merge(theirs, message).await?;

	for path in conflicts {
		println!("CONFLICT (content): Merge conflict in {path}");
	}
	// Signal the conflict as a typed outcome; the front-end turns it into a non-zero exit (`gta`) or a
	// tool error (`gta-mcp`). A library function must not decide the process's fate with `exit`, which
	// would terminate a long-lived `gta-mcp` server.
	Err(crate::MergeConflict.into())
}

/// Conclude an in-progress merge: a two-parent commit from the resolved index. Shared by
/// `merge --continue` (`message_override = None`, uses `MERGE_MSG`) and `gta commit` during a merge
/// (`message_override = Some(..)`). Refuses while the index still has unmerged stages.
pub(crate) async fn complete_merge<H: HashAlgorithm>(
	wt: &WorkTree<LocalFileStore, H>,
	message_override: Option<String>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(merge_head) = repository.merge_head().await? else {
		bail!("there is no merge in progress (MERGE_HEAD is missing)");
	};

	let index = wt.load_index()?;
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}
	// An empty index is a valid result here (e.g. a delete/modify conflict resolved by deleting the
	// file): git records a two-parent merge commit with an empty tree, unlike an ordinary commit.
	let entries = index_tree_entries(&index);
	let tree = repository.write_tree(&entries).await?;
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
async fn abort_merge<H: HashAlgorithm>(wt: &WorkTree<LocalFileStore, H>) -> Result<()> {
	let repository = wt.repository();
	if repository.merge_head().await?.is_none() {
		bail!("there is no merge to abort (MERGE_HEAD is missing)");
	}
	// HEAD does not move while a merge is in progress, so its tree is the pre-merge state.
	let Some(head) = repository.refs().resolve_head().await? else {
		bail!("there is no merge to abort (HEAD is unborn)");
	};
	let head_tree = repository.commit_tree(head).await?;
	wt.checkout(head_tree, true).await?;
	repository.clear_merge().await?;
	Ok(())
}

/// git's default merge message: `Merge branch '<name>'` when the argument names a local branch,
/// otherwise `Merge commit '<arg>'`.
async fn default_message<H: HashAlgorithm>(
	repository: &Repository<LocalFileStore, H>,
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

/// A tree's entries as `path -> (mode, oid)`, for recording conflict stages.
async fn tree_entry_map<H: HashAlgorithm>(
	repository: &Repository<LocalFileStore, H>,
	tree: ObjectId<H>,
) -> Result<HashMap<String, (u32, ObjectId<H>)>> {
	let mut map = HashMap::new();
	for (path, mode, oid) in repository.read_tree(tree).await? {
		let mode = u32::from_str_radix(&mode, 8).unwrap_or(0o100644);
		map.insert(path, (mode, oid));
	}
	Ok(map)
}

fn ensure_trailing_newline(message: String) -> String {
	if message.ends_with('\n') {
		message
	} else {
		format!("{message}\n")
	}
}

/// Reduce the merge bases to a single base tree for the three-way merge. With one base it is that
/// commit's tree; with several (a criss-cross history) the base trees are folded together, each
/// against its own common ancestor, into a "virtual" base — the way git's recursive strategy does.
/// (Deeply nested criss-crosses are approximated: the virtual base is not re-inserted into the
/// commit graph for later merge-base queries.)
async fn virtual_base_tree<H: HashAlgorithm>(
	repository: &Repository<LocalFileStore, H>,
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
