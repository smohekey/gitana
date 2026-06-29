use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::ObjectId;
use gitana_repository::HeadState;

use crate::identity;
use crate::repo::{self, LocalRepository};

/// Merge `commit` into the current branch.
///
/// Fast-forwards when the current branch is an ancestor of `commit` (unless `--no-ff`), otherwise
/// creates a true two-parent merge commit. `--ff-only` refuses a non-fast-forward. A merge that
/// would conflict is detected and refused, leaving the work tree, index, and `HEAD` untouched
/// (materialising conflicts — `MERGE_HEAD`, the conflicted index — is a separate step).
pub async fn run(
	cwd: &Path,
	commit: String,
	message: Option<String>,
	no_ff: bool,
	ff_only: bool,
) -> Result<()> {
	if no_ff && ff_only {
		bail!("--no-ff and --ff-only are incompatible");
	}

	let (_, git_dir) = repo::discover(cwd)?;
	let wt = repo::open_worktree(cwd)?;
	let repository = wt.repository();

	// Refuse if a previous merge has not been concluded: `MERGE_HEAD` persists until the merge is
	// committed or aborted, even once its conflicts have been resolved and the index is clean again.
	// (cherry-pick/revert state files would be added with those commands.)
	if git_dir.join("MERGE_HEAD").exists() {
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
	if can_fast_forward && (!no_ff || head_tip.is_none()) {
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

	if ff_only {
		bail!("not possible to fast-forward, aborting");
	}

	// A real merge commit needs the current tip as its first parent.
	let head = head_tip.expect("non-fast-forward merge has a current commit");

	// The merged tree: `theirs` itself for a `--no-ff` of a fast-forwardable history, otherwise the
	// three-way merge of the branch and `commit` against their best common ancestor.
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
			bail!(
				"merge conflict in {}; merging with conflicts is not yet supported",
				merge.conflicts.join(", ")
			);
		}
		merge.tree
	};

	let author = identity::signature(repository, "AUTHOR").await?;
	let committer = identity::signature(repository, "COMMITTER").await?;
	let message = match message {
		Some(message) => message,
		None => default_message(repository, &commit).await,
	};
	let message = if message.ends_with('\n') {
		message
	} else {
		format!("{message}\n")
	};

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

/// git's default merge message: `Merge branch '<name>'` when the argument names a local branch,
/// otherwise `Merge commit '<arg>'`.
async fn default_message(repository: &LocalRepository, arg: &str) -> String {
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
async fn virtual_base_tree(repository: &LocalRepository, bases: &[ObjectId]) -> Result<ObjectId> {
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

fn short(id: ObjectId) -> String {
	let hex = id.to_hex();
	hex[..12.min(hex.len())].to_owned()
}
