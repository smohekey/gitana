//! `merge` — merge a commit into the current branch (fast-forward or a true two-parent merge),
//! conclude an in-progress merge, or abort one. The start path returns a [`MergeOutcome`] that the
//! CLI adapter renders; a conflict materialises in-progress state and reports its paths as data
//! rather than printing.

use std::future::Future;

use anyhow::{Context, Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::{ReflogIntent, Repository};
use gitana_worktree::{WorkTree, WorktreeError};

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
	/// A true merge was refused because rebuilding the index would overwrite staged changes. No
	/// merge state was recorded; the caller renders the byte-preserving paths and signals failure.
	WouldOverwrite { paths: Vec<gitana_path::GitPath> },
	/// The merge conflicted; an in-progress merge has been materialised (`MERGE_HEAD`, `MERGE_MSG`, a
	/// conflicted index and work tree). The caller renders the conflicted paths and signals failure.
	Conflict { paths: Vec<gitana_path::GitPath> },
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
	merge_inner(
		wt,
		commit_spec,
		(message, || async { Ok(None) }),
		no_ff,
		ff_only,
		identity,
		signer,
	)
	.await
}

/// Merge `commit_spec` with the default merge message, using the caller-resolved global excludes
/// file when deciding whether an untracked obstruction may be overwritten. Repository-local
/// `.gitignore` and `.git/info/exclude` inputs are still resolved by the worktree engine.
pub async fn merge_with_excludes<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	commit_spec: &str,
	excludes_file: Option<&str>,
	no_ff: bool,
	ff_only: bool,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<MergeOutcome<H>> {
	let excludes_file = excludes_file.map(|value| value.as_bytes().to_vec());
	merge_inner(
		wt,
		commit_spec,
		(None, || async move { Ok(excludes_file) }),
		no_ff,
		ff_only,
		identity,
		signer,
	)
	.await
}

/// Merge `commit_spec` while resolving the caller's global excludes only if checkout or conflict
/// materialisation is required.
///
/// The one-shot loader runs under the locked HEAD snapshot, after the already-up-to-date and other
/// graph-only outcomes have been decided. Repository-local `.gitignore` and `.git/info/exclude`
/// inputs remain the worktree engine's responsibility.
pub async fn merge_with_excludes_loader<
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
	S: Signer,
	L: FnOnce() -> LF,
	LF: Future<Output = Result<Option<Vec<u8>>>>,
>(
	wt: &WorkTree<F, W, H>,
	commit_spec: &str,
	load_excludes: L,
	no_ff: bool,
	ff_only: bool,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<MergeOutcome<H>> {
	merge_inner(
		wt,
		commit_spec,
		(None, load_excludes),
		no_ff,
		ff_only,
		identity,
		signer,
	)
	.await
}

async fn merge_inner<
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
	S: Signer,
	L: FnOnce() -> LF,
	LF: Future<Output = Result<Option<Vec<u8>>>>,
>(
	wt: &WorkTree<F, W, H>,
	commit_spec: &str,
	message_and_excludes: (Option<String>, L),
	no_ff: bool,
	ff_only: bool,
	identity: &impl Identity,
	signer: Option<&S>,
) -> Result<MergeOutcome<H>> {
	let (message, load_excludes) = message_and_excludes;
	let mut load_excludes = Some(load_excludes);
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

	let mut head_transaction = repository.refs().lock_head_transaction().await?;
	let theirs = repository
		.rev_parse(&format!("{commit_spec}^{{commit}}"))
		.await?;
	// The transaction pins both the exact HEAD spelling and its starting branch through worktree and
	// ref publication. A concurrent checkout therefore cannot redirect the eventual reset to another
	// same-tip branch while signing is in flight.
	let head_tip = head_transaction.tip();

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
		let excludes_file = load_merge_excludes(&mut load_excludes).await?;
		// Apply only the HEAD→theirs diff (git's two-tree merge), so unrelated staged or dirty files
		// survive; a local change to a path the fast-forward updates is refused, not clobbered. This is the
		// same lock-safe, D/F- and sparse-correct engine as `switch` — it aborts on the FIRST conflicting
		// path (git names one), rather than the pre-scanned full list the retired `twoway_merge` returned.
		let from_tree = match head_tip {
			Some(head) => repository.commit_tree(head).await?,
			None => repository.write_tree(&[]).await?,
		};
		// The two-tree merge assumes the index reflects the `from_tree`. If `.git/index` is missing,
		// `checkout_merge` would instead fall back to the authoritative overlay: on a born branch that could
		// advance HEAD while leaving a to-be-removed tracked file untracked (git and the retired `twoway_merge`
		// refuse this), and on an unborn branch the overlay skips validating sparse-excluded target blobs, so a
		// missing out-of-cone blob would still publish an unmaterialisable commit. Build the from-index first —
		// HEAD's tree on a born branch, the empty tree when unborn — so the fast-forward always runs through
		// `merge_apply` (which validates every target blob). The rebuild is atomic under the index lock (a
		// no-op if the index exists), so it cannot discard a concurrent writer's staged work.
		let committer = identity.committer_or_default().await?;
		let reflog_message = format!("merge {commit_spec}: Fast-forward");
		head_transaction
			.prepare_reset(
				theirs,
				ReflogIntent::Log {
					committer: &committer,
					message: &reflog_message,
				},
			)
			.await?;
		wt.ensure_index_from_tree_if_missing(from_tree).await?;
		match wt
			.checkout_merge(from_tree, theirs_tree, excludes_file.as_deref())
			.await
		{
			Ok(()) => {}
			Err(WorktreeError::Conflict(path)) | Err(WorktreeError::UntrackedOverwrite(path)) => {
				return Ok(MergeOutcome::WouldOverwrite { paths: vec![path] });
			}
			Err(error) => return Err(error.into()),
		}
		head_transaction.finish().await?;
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
		return Ok(MergeOutcome::WouldOverwrite { paths: staged });
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
			let excludes_file = load_merge_excludes(&mut load_excludes).await?;
			// Materialise the conflict for the user to resolve: conflicted work tree and index,
			// `ORIG_HEAD`, then `MERGE_HEAD`/`MERGE_MSG`.
			conflict::write_conflicted_state_with_excludes(
				wt,
				merge.tree,
				base_tree,
				head_tree,
				theirs_tree,
				&merge.conflicts,
				excludes_file.as_deref(),
			)
			.await?;
			head_transaction.finish_orig_head().await?;
			repository.start_merge(theirs, &message).await?;
			return Ok(MergeOutcome::Conflict {
				paths: merge.conflicts,
			});
		}
		merge.tree
	};
	let excludes_file = load_merge_excludes(&mut load_excludes).await?;

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
	let reflog_message = format!("merge {commit_spec}: Merge made by the 'recursive' strategy.");
	head_transaction
		.prepare_reset(
			merge_commit,
			ReflogIntent::Log {
				committer: &committer,
				message: &reflog_message,
			},
		)
		.await?;
	// Two-tree merge from HEAD's tree to the merged result: the index equals HEAD here (guarded above), so
	// this lays down the merge while preserving unrelated local work and refusing a real conflict, sharing
	// `switch`'s lock-safe, D/F- and sparse-correct engine.
	wt.checkout_merge(head_tree, merged_tree, excludes_file.as_deref())
		.await?;
	head_transaction.finish().await?;
	Ok(MergeOutcome::Made {
		commit: merge_commit,
	})
}

async fn load_merge_excludes<L, LF>(loader: &mut Option<L>) -> Result<Option<Vec<u8>>>
where
	L: FnOnce() -> LF,
	LF: Future<Output = Result<Option<Vec<u8>>>>,
{
	(loader
		.take()
		.expect("each merge path resolves excludes at most once"))()
	.await
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

/// The paths that differ between two trees (added/removed/modified), sorted.
async fn tree_diff_paths<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	a: ObjectId<H>,
	b: ObjectId<H>,
) -> Result<Vec<gitana_path::GitPath>> {
	use std::collections::{HashMap, HashSet};
	let map = |entries: Vec<(gitana_path::GitPath, String, ObjectId<H>)>| {
		entries
			.into_iter()
			.map(|(path, mode, oid)| (path, (mode, oid)))
			.collect::<HashMap<_, _>>()
	};
	let am = map(repository.read_tree(a).await?);
	let bm = map(repository.read_tree(b).await?);
	let mut paths: Vec<gitana_path::GitPath> = am
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
	use std::sync::atomic::{AtomicBool, Ordering};

	use gitana_path::GitPath;
	use gitana_worktree::{IndexEntry, Stat};

	use super::*;
	use crate::test_support::{
		FailingSigner, TestIdentity, TestSigner, commit_file, fixture, loose_commit,
	};
	use gitana_file_store::FileStore;
	use gitana_file_store_local::LocalFileStore;
	use gitana_object::Sha256;
	use gitana_repository::{HeadState, RefStore};

	struct BranchSwitchingSigner {
		files: LocalFileStore,
		signer: TestSigner,
	}

	impl Signer for BranchSwitchingSigner {
		async fn sign(&self, payload: &[u8]) -> Result<String> {
			let refs: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&self.files);
			let error = refs
				.set_symbolic("HEAD", "refs/heads/other", ReflogIntent::Skip)
				.await
				.expect_err("the merge must retain HEAD while signing");
			assert!(
				matches!(&error, gitana_repository::RepositoryError::HistoryLocked),
				"expected repository history contention, got {error:?}",
			);
			self.signer.sign(payload).await
		}
	}

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
	async fn already_up_to_date_does_not_resolve_excludes() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let target = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		let called = AtomicBool::new(false);

		let outcome = merge_with_excludes_loader(
			&wt,
			&target.to_hex(),
			|| async {
				called.store(true, Ordering::SeqCst);
				bail!("excludes must stay lazy for an already-current merge")
			},
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();

		assert!(matches!(outcome, MergeOutcome::AlreadyUpToDate));
		assert!(!called.load(Ordering::SeqCst));
	}

	#[tokio::test]
	async fn failed_merge_prunes_directories_created_for_an_unborn_head_lock() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let refs = wt.repository().refs();
		refs
			.set_head_symbolic("refs/heads/team/topic", ReflogIntent::Skip)
			.await
			.unwrap();

		merge(
			&wt,
			"does-not-exist",
			None,
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap_err();

		assert!(!dir.path().join(".git/refs/heads/team").exists());
		assert!(dir.path().join(".git/refs/heads").is_dir());
		refs
			.set_head_symbolic("refs/heads/team", ReflogIntent::Skip)
			.await
			.unwrap();
		let commit = commit_file(dir.path(), &wt, "after.txt", b"after\n", &id).await;
		assert_eq!(refs.resolve("refs/heads/team").await.unwrap(), Some(commit));
	}

	#[tokio::test]
	async fn staged_refusal_preserves_distinct_raw_paths_for_the_frontend() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let base = commit_file(dir.path(), &wt, "base.txt", b"base\n", &id).await;
		let ours = commit_file(dir.path(), &wt, "ours.txt", b"ours\n", &id).await;
		let theirs = loose_commit(wt.repository(), vec![base], "theirs.txt", b"theirs\n").await;
		let raw = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let blob = wt.repository().write_blob(b"staged\n").await.unwrap();
		let mut index = wt.load_index().await.unwrap();
		for path in [&raw, &literal] {
			index.upsert(IndexEntry {
				stat: Stat::default(),
				mode: 0o100644,
				oid: blob,
				stage: 0,
				assume_valid: false,
				skip_worktree: false,
				intent_to_add: false,
				path: path.clone(),
			});
		}
		wt.save_index(&index).await.unwrap();

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
		let MergeOutcome::WouldOverwrite { paths } = outcome else {
			panic!("expected a staged-change refusal");
		};
		assert_eq!(paths.len(), 2);
		assert!(paths.contains(&raw));
		assert!(paths.contains(&literal));
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(ours)
		);
		assert_eq!(wt.repository().merge_head().await.unwrap(), None);
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
	async fn fast_forward_preserves_a_symbolic_head_chain() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let a = commit_file(dir.path(), &wt, "f.txt", b"a\n", &id).await;
		let b = loose_commit(wt.repository(), vec![a], "f.txt", b"b\n").await;
		let refs = wt.repository().refs();
		refs
			.set_symbolic("refs/heads/alias", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();
		refs
			.set_head_symbolic("refs/heads/alias", ReflogIntent::Skip)
			.await
			.unwrap();

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

		assert!(matches!(outcome, MergeOutcome::FastForward { to, .. } if to == b));
		assert_eq!(
			refs.read_head().await.unwrap(),
			gitana_repository::HeadState::Symbolic("refs/heads/alias".to_owned())
		);
		assert_eq!(
			refs
				.read_symbolic("refs/heads/alias")
				.await
				.unwrap()
				.as_deref(),
			Some("refs/heads/main")
		);
		assert_eq!(refs.resolve("refs/heads/main").await.unwrap(), Some(b));
		assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"b\n");
	}

	#[tokio::test]
	async fn merge_excludes_allow_ignored_untracked_obstructions() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let base = commit_file(dir.path(), &wt, "base.txt", b"base\n", &id).await;
		let target = loose_commit(wt.repository(), vec![base], "ignored.txt", b"upstream\n").await;
		std::fs::write(dir.path().join("ignored.txt"), b"local obstruction\n").unwrap();

		let outcome = merge_with_excludes(
			&wt,
			&target.to_hex(),
			Some("ignored.txt\n"),
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();

		assert!(matches!(outcome, MergeOutcome::FastForward { to, .. } if to == target));
		assert_eq!(
			std::fs::read(dir.path().join("ignored.txt")).unwrap(),
			b"upstream\n"
		);
	}

	#[tokio::test]
	async fn true_merge_uses_excludes_for_checkout_obstructions() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let base = commit_file(dir.path(), &wt, "base.txt", b"base\n", &id).await;
		let _ours = commit_file(dir.path(), &wt, "ours.txt", b"ours\n", &id).await;
		let target = loose_commit(wt.repository(), vec![base], "ignored.txt", b"upstream\n").await;
		std::fs::write(dir.path().join("ignored.txt"), b"local obstruction\n").unwrap();

		let outcome = merge_with_excludes(
			&wt,
			&target.to_hex(),
			Some("ignored.txt\n"),
			false,
			false,
			&id,
			None::<&TestSigner>,
		)
		.await
		.unwrap();

		assert!(matches!(outcome, MergeOutcome::Made { .. }));
		assert_eq!(
			std::fs::read(dir.path().join("ignored.txt")).unwrap(),
			b"upstream\n"
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
	async fn signed_merge_cannot_publish_to_a_concurrently_selected_same_tip_branch() {
		let (dir, wt) = fixture().await;
		let id = TestIdentity::default();
		let base = commit_file(dir.path(), &wt, "base.txt", b"base\n", &id).await;
		let ours = commit_file(dir.path(), &wt, "ours.txt", b"ours\n", &id).await;
		let theirs = loose_commit(wt.repository(), vec![base], "theirs.txt", b"theirs\n").await;
		wt.repository()
			.refs()
			.update_ref("refs/heads/other", ours, None, ReflogIntent::Skip)
			.await
			.unwrap();
		let signer = BranchSwitchingSigner {
			files: wt.repository().objects().file_store().shared_handle(),
			signer: TestSigner::new(7),
		};

		let outcome = merge(
			&wt,
			&theirs.to_hex(),
			None,
			false,
			false,
			&id,
			Some(&signer),
		)
		.await
		.unwrap();
		let MergeOutcome::Made { commit } = outcome else {
			panic!("expected a merge commit");
		};
		assert_eq!(
			wt.repository().refs().read_head().await.unwrap(),
			HeadState::Symbolic("refs/heads/main".to_owned())
		);
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(commit)
		);
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/other")
				.await
				.unwrap(),
			Some(ours)
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
