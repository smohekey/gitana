//! `prune` / `gc` — reclaim storage safely.
//!
//! `prune` deletes loose objects unreachable from *every* root (refs, HEAD, the index, the
//! reflogs, and any in-progress-operation head); `gc` runs a repack then a prune. The safety rule
//! is conservative: an object is removed only when nothing can still reach it, and prune refuses to
//! run while an operation is in progress. There is no time-based grace period (the file store
//! exposes no mtime), so prune is an explicit, quiescent-repo operation. Reflog entries are roots,
//! so a commit a reflog can still reach (e.g. before a `reset`) is kept until the reflog is trimmed.

use std::collections::HashSet;

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_tag, referenced_ids};
use gitana_object_store::{BitmapReport, ObjectStoreError, PruneReport, RepackReport};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// Delete loose objects that no root can reach. Refuses while a merge, cherry-pick, revert, or
/// rebase is in progress — those objects are protected as roots, but a half-applied tree is no
/// time to prune.
pub async fn prune<F: FileStore, H: HashAlgorithm>(wt: &WorkTree<F, H>) -> Result<PruneReport> {
	let repo = wt.repository();
	refuse_if_linked_worktrees(repo).await?;
	refuse_if_operation_in_progress(repo).await?;
	let roots = collect_roots(wt).await?;
	let reachable = reachable_from(repo, roots).await?;
	Ok(repo.objects().prune_loose(&reachable).await?)
}

/// git's default geometric growth factor, used by `gc`'s incremental repack.
const GEOMETRIC_FACTOR: u64 = 2;

/// Prune, then incrementally repack, then write a reachability bitmap. Prune must run *first* —
/// repack packs every reachable object, which would move an unreachable loose object out of prune's
/// loose-only reach and defeat the reclaim. The repack is *geometric*: it keeps the large packs and
/// rolls only the small packs + loose into new ones, so `gc` stays cheap as history grows (use
/// `gta repack` for a full consolidation). Finally the ref tips are bitmapped so later reachability
/// queries (fetch negotiation, `rev-list`) can skip the history walk.
pub async fn gc<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
) -> Result<(PruneReport, Option<RepackReport>, Option<BitmapReport>)> {
	let prune = prune(wt).await?;
	let repo = wt.repository();
	let max_pack_size = repo.pack_size_limit().await?;
	let repack = repo
		.objects()
		.repack_geometric(max_pack_size, GEOMETRIC_FACTOR)
		.await?;
	// The store keeps only the packed commits among these tips (a tag object or loose tip is skipped).
	let tips = ref_tip_ids(wt).await?;
	let bitmap = repo.objects().write_reachability_bitmap(&tips).await?;
	Ok((prune, repack, bitmap))
}

/// The commits the refs and `HEAD` point at — the tips worth bitmapping (git bitmaps ref tips).
/// Covers direct refs, symbolic-ref targets, and `HEAD` (the same roots `prune` protects), and
/// peels an annotated tag to the commit it names (as git does), so a tag on an otherwise unselected
/// commit still gets bitmapped. Deduplicated; the object store filters to packed commits.
async fn ref_tip_ids<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
) -> Result<Vec<ObjectId<H>>> {
	let repo = wt.repository();
	let refs = repo.refs();
	let mut raw: Vec<ObjectId<H>> = refs
		.list("refs/")
		.await?
		.into_iter()
		.map(|(_, id)| id)
		.collect();
	raw.extend(refs.symbolic_ref_targets("refs/").await?);
	raw.extend(refs.resolve_head().await?);

	let mut commits: HashSet<ObjectId<H>> = HashSet::new();
	for id in raw {
		if let Some(commit) = peel_to_commit(repo, id).await? {
			commits.insert(commit);
		}
	}
	Ok(commits.into_iter().collect())
}

/// Follow annotated-tag objects to the commit they name, returning that commit (or `None` if the
/// ref resolves to a tree/blob, is missing, or a tag chain cycles).
async fn peel_to_commit<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	start: ObjectId<H>,
) -> Result<Option<ObjectId<H>>> {
	let mut current = start;
	let mut seen: HashSet<ObjectId<H>> = HashSet::new();
	loop {
		if !seen.insert(current) {
			return Ok(None); // a malformed tag chain that loops
		}
		match repo.objects().read_object(&current).await {
			Ok((ObjectKind::Commit, _)) => return Ok(Some(current)),
			Ok((ObjectKind::Tag, data)) => current = parse_tag::<H>(&data)?.object,
			Ok(_) | Err(ObjectStoreError::NotFound) => return Ok(None),
			Err(other) => return Err(other.into()),
		}
	}
}

/// Each linked worktree (`git worktree add`) keeps its own `HEAD`, index, reflog, and
/// in-progress-operation state under the common dir's `worktrees/` — roots this single-worktree
/// walk does not scan, and per-worktree refs live in a different git dir than the shared `refs/`
/// this walk lists. Pruning could therefore delete an object another worktree still references, so
/// refuse whenever any linked worktree exists (full multi-worktree gc is future work).
async fn refuse_if_linked_worktrees<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<()> {
	if !repo
		.objects()
		.file_store()
		.list_prefix("worktrees/")
		.await?
		.is_empty()
	{
		bail!(
			"cannot prune: this repository has linked worktrees; multi-worktree gc is not yet supported"
		);
	}
	Ok(())
}

async fn refuse_if_operation_in_progress<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<()> {
	// A rebase started by stock git records its state under `rebase-merge/` (merge/interactive
	// backend) or `rebase-apply/` (am backend), not gitana's flat `REBASE_*` files — so detect
	// both. Mid-rebase, git may hold a just-replayed commit that no ref/reflog references yet;
	// pruning then could delete it.
	let files = repo.objects().file_store();
	let in_progress = repo.merge_head().await?.is_some()
		|| repo.cherry_pick_head().await?.is_some()
		|| repo.revert_head().await?.is_some()
		|| repo.rebase_in_progress().await?
		|| files.exists("rebase-merge/head-name").await?
		|| files.exists("rebase-apply/next").await?;
	if in_progress {
		bail!("cannot prune while an operation is in progress; finish or abort it first");
	}
	Ok(())
}

/// Every root a prune must keep reachable: refs (direct *and* symbolic-ref targets), HEAD, the
/// index (all stages, so a staged-but-uncommitted blob survives), the reflogs, and any
/// in-progress-operation head (`ORIG_HEAD` and the merge / cherry-pick / revert / rebase heads).
async fn collect_roots<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
) -> Result<Vec<ObjectId<H>>> {
	let repo = wt.repository();
	let refs = repo.refs();
	let mut roots: Vec<ObjectId<H>> = Vec::new();

	roots.extend(refs.list("refs/").await?.into_iter().map(|(_, id)| id));
	roots.extend(refs.symbolic_ref_targets("refs/").await?);
	roots.extend(refs.resolve_head().await?);
	roots.extend(refs.reflog_object_ids().await?);
	roots.extend(repo.orig_head().await?);
	roots.extend(repo.merge_head().await?);
	roots.extend(repo.cherry_pick_head().await?);
	roots.extend(repo.revert_head().await?);
	if let Some(state) = repo.rebase_state().await? {
		roots.push(state.orig_head);
		roots.push(state.onto);
		roots.extend(state.todo);
	}
	roots.extend(wt.load_index()?.entries.iter().map(|entry| entry.oid));

	Ok(roots)
}

/// The transitive closure of `roots` over the object graph — each object's [`referenced_ids`],
/// read loose or packed. A root that does not exist is skipped (an empty repo has none), matching
/// the async walk used to build a fetch pack.
async fn reachable_from<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	roots: Vec<ObjectId<H>>,
) -> Result<HashSet<ObjectId<H>>> {
	let store = repo.objects();
	let mut reachable: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack = roots;
	while let Some(id) = stack.pop() {
		if !reachable.insert(id) {
			continue;
		}
		match store.read_object(&id).await {
			Ok((kind, data)) => stack.extend(referenced_ids::<H>(kind, &data)?),
			Err(ObjectStoreError::NotFound) => {}
			Err(other) => return Err(other.into()),
		}
	}
	Ok(reachable)
}
