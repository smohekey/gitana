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
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_commit, parse_tag, referenced_ids};
use gitana_object_store::{BitmapReport, ObjectStoreError, PruneReport, RepackReport};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// Delete loose objects that no root can reach. Refuses while a merge, cherry-pick, revert, or
/// rebase is in progress — those objects are protected as roots, but a half-applied tree is no
/// time to prune.
pub async fn prune<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<PruneReport> {
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
pub async fn gc<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<(PruneReport, Option<RepackReport>, Option<BitmapReport>)> {
	let prune = prune(wt).await?;
	let repo = wt.repository();
	let max_pack_size = repo.pack_size_limit().await?;
	let repack = repo
		.objects()
		.repack_geometric(max_pack_size, GEOMETRIC_FACTOR)
		.await?;
	// A shallow repository's history stops at the `.git/shallow` boundary, so its objects are not the
	// complete reachability closure a bitmap encodes — a boundary commit's parents are absent. Building
	// a bitmap would walk into those missing parents; git likewise declines bitmaps on a shallow repo.
	// Skip it (the bitmap is only a query accelerator, so a shallow repo simply runs without one).
	let bitmap = if repo.read_shallow().await?.is_empty() {
		// The store keeps only the packed commits among these tips (a tag object or loose tip is skipped).
		let tips = ref_tip_ids(wt).await?;
		repo.objects().write_reachability_bitmap(&tips).await?
	} else {
		None
	};
	Ok((prune, repack, bitmap))
}

/// The commits the refs and `HEAD` point at — the tips worth bitmapping (git bitmaps ref tips).
/// Covers direct refs, symbolic-ref targets, and `HEAD` (the same roots `prune` protects), and
/// peels an annotated tag to the commit it names (as git does), so a tag on an otherwise unselected
/// commit still gets bitmapped. Deduplicated; the object store filters to packed commits.
async fn ref_tip_ids<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
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
/// index (all stages, so a staged-but-uncommitted blob survives), the reflogs, any
/// in-progress-operation head (`ORIG_HEAD` and the merge / cherry-pick / revert / rebase heads), and
/// every `.git/shallow` boundary commit — a shallow entry is a commit the client *has* (only its
/// parents are withheld), and it is what the client re-sends as a `shallow` line on a later
/// deepen/`--unshallow`, so deleting it would leave `.git/shallow` naming a missing object. (The walk
/// still stops *at* a boundary, so history past it is reclaimed; the boundary itself is kept.)
async fn collect_roots<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Vec<ObjectId<H>>> {
	let repo = wt.repository();
	let refs = repo.refs();
	let mut roots: Vec<ObjectId<H>> = Vec::new();

	roots.extend(refs.list("refs/").await?.into_iter().map(|(_, id)| id));
	roots.extend(refs.symbolic_ref_targets("refs/").await?);
	roots.extend(refs.resolve_head().await?);
	roots.extend(refs.reflog_object_ids().await?);
	roots.extend(repo.read_shallow().await?);
	roots.extend(repo.orig_head().await?);
	roots.extend(repo.merge_head().await?);
	roots.extend(repo.cherry_pick_head().await?);
	roots.extend(repo.revert_head().await?);
	if let Some(state) = repo.rebase_state().await? {
		roots.push(state.orig_head);
		roots.push(state.onto);
		roots.extend(state.todo);
	}
	roots.extend(wt.load_index().await?.entries.iter().map(|entry| entry.oid));

	Ok(roots)
}

/// The transitive closure of `roots` over the object graph — each object's [`referenced_ids`], read
/// loose or packed.
///
/// The walk respects `.git/shallow`: a boundary commit is treated as parentless (only its tree is
/// enqueued), so history past the boundary is excluded even when those parent objects still exist on
/// disk — as after `fetch --depth` truncates a full clone. A missing object is also a dead end
/// (skipped), covering an empty repo's absent roots and a shallow clone's genuinely-absent parents.
/// This matches the shallow view git presents, so prune/gc reclaim truncated history and `fetch`'s tag
/// auto-follow (which tests reachability from the fetched branch tips) does not follow tags past the
/// boundary.
///
/// When the repo is non-shallow and a usable reachability bitmap is present, this delegates to
/// [`ObjectStore::reachable_object_closure`] (git's bitmap fill-in: a bitmapped commit contributes its
/// whole closure in one step, only the un-bitmapped frontier is walked) — the same reachable set the
/// walk below computes, since both skip a missing object and follow the identical [`referenced_ids`]
/// edges. The gate mirrors slices 1-2: a bitmap is only written for a non-shallow repo, and it encodes
/// full-history reachability that would cross a shallow boundary the walk must respect.
pub(crate) async fn reachable_from<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	roots: Vec<ObjectId<H>>,
) -> Result<HashSet<ObjectId<H>>> {
	let store = repo.objects();
	let shallow: HashSet<ObjectId<H>> = repo.read_shallow().await?.into_iter().collect();
	// The bitmap encodes full-history reachability (crossing any shallow boundary) and exists only for a
	// non-shallow repo, so take the accelerated closure only with no boundary to respect. Lenient: a
	// missing object is skipped, matching the walk below.
	if shallow.is_empty() && store.has_reachability_bitmap().await? {
		return Ok(store.reachable_object_closure(&roots, false).await?);
	}
	let mut reachable: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack = roots;
	while let Some(id) = stack.pop() {
		if !reachable.insert(id) {
			continue;
		}
		match store.read_object(&id).await {
			// A shallow-boundary commit is parentless here: enqueue only its tree, not the parents past
			// the boundary (which may still be on disk).
			Ok((ObjectKind::Commit, data)) if shallow.contains(&id) => {
				stack.push(parse_commit::<H>(&data)?.tree);
			}
			Ok((kind, data)) => stack.extend(referenced_ids::<H>(kind, &data)?),
			Err(ObjectStoreError::NotFound) => {}
			Err(other) => return Err(other.into()),
		}
	}
	Ok(reachable)
}

#[cfg(test)]
mod tests {
	use gitana_object::{ObjectId, ObjectKind};

	use super::*;
	use crate::test_support::{fixture, loose_commit};

	/// A shallow repository holds commits whose parents are deliberately absent (the `.git/shallow`
	/// boundary). `prune`/`gc` must treat those parents as a dead end, not corruption — the reachability
	/// walk already stops at any missing object, so a shallow clone can safely run maintenance.
	#[tokio::test]
	async fn prune_and_gc_tolerate_a_shallow_boundary() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();

		// A boundary commit: its parent object is not in the store, as after a shallow fetch.
		let absent_parent = ObjectId::compute(ObjectKind::Commit, b"absent-boundary-parent");
		let tip = loose_commit(repo, vec![absent_parent], "f.txt", b"x").await;
		repo
			.refs()
			.update_ref(
				"refs/heads/main",
				tip,
				None,
				gitana_repository::ReflogIntent::Skip,
			)
			.await
			.unwrap();
		repo.write_shallow(&[tip]).await.unwrap();

		// Neither operation may choke walking into the absent parent, and the boundary tip survives.
		prune(&wt).await.unwrap();
		assert!(repo.objects().exists_object(&tip).await.unwrap());
		gc(&wt).await.unwrap();
		assert!(repo.objects().exists_object(&tip).await.unwrap());
	}

	/// The reachability walk stops at a `.git/shallow` boundary even when the parent objects are still on
	/// disk (as after `fetch --depth` truncates a full clone) — the shallow view git presents, so prune
	/// can reclaim the truncated history and auto-follow does not chase tags past the boundary.
	#[tokio::test]
	async fn reachable_from_stops_at_a_present_shallow_boundary() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();

		// A real parent<-child chain: both commit objects are present on disk.
		let parent = loose_commit(repo, Vec::new(), "f.txt", b"p").await;
		let child = loose_commit(repo, vec![parent], "f.txt", b"c").await;
		// Mark the child a shallow boundary, as a depth-1 truncation would.
		repo.write_shallow(&[child]).await.unwrap();

		let reachable = reachable_from(repo, vec![child]).await.unwrap();
		assert!(reachable.contains(&child), "the boundary tip is reachable");
		assert!(
			!reachable.contains(&parent),
			"the walk stops at the boundary, not chasing the still-present parent"
		);
		assert!(
			repo.objects().exists_object(&parent).await.unwrap(),
			"the parent object is genuinely still on disk"
		);
	}

	/// With a usable reachability bitmap on a non-shallow repo, `reachable_from` takes the accelerated
	/// closure ([`ObjectStore::reachable_object_closure`]) instead of the graph walk. It must compute the
	/// same reachable set either way — the whole safety of prune/gc liveness rests on that equivalence.
	/// The graph mixes a bitmapped tip (whose closure the bitmap supplies in one step) with an
	/// un-bitmapped side tip (whose objects the frontier walk must still fill in).
	#[tokio::test]
	async fn reachable_from_with_a_bitmap_matches_the_walk() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();

		// c0 <- c1 <- c2 (main tip) and c1 <- c3 (side tip); distinct content ⇒ distinct trees/blobs.
		let c0 = loose_commit(repo, Vec::new(), "f.txt", b"0").await;
		let c1 = loose_commit(repo, vec![c0], "f.txt", b"1").await;
		let c2 = loose_commit(repo, vec![c1], "f.txt", b"2").await;
		let c3 = loose_commit(repo, vec![c1], "f.txt", b"3").await;
		let roots = vec![c2, c3];

		// Baseline: no bitmap yet, so this runs the graph walk.
		assert!(
			!repo.objects().has_reachability_bitmap().await.unwrap(),
			"no bitmap before repack",
		);
		let from_walk = reachable_from(repo, roots.clone()).await.unwrap();

		// Repack and bitmap only the main tip c2; c3's objects are left to the frontier walk-fill.
		repo.objects().repack(u64::MAX).await.unwrap();
		repo
			.objects()
			.write_reachability_bitmap(&[c2])
			.await
			.unwrap();
		assert!(
			repo.objects().has_reachability_bitmap().await.unwrap(),
			"the repo should now have a reachability bitmap",
		);

		// With the bitmap present (and no shallow boundary) this takes the closure fast path.
		let from_bitmap = reachable_from(repo, roots).await.unwrap();

		assert_eq!(
			from_walk, from_bitmap,
			"the bitmap closure must equal the graph walk",
		);
		// Sanity: the set is the whole history, including the un-bitmapped side tip filled in by the walk.
		for id in [c0, c1, c2, c3] {
			assert!(from_bitmap.contains(&id), "commit {id} reachable");
		}
	}

	/// Two bitmapped tips that share ancestry: the accelerated closure ORs each tip's reachability
	/// bitmap in place — so the shared closure (c0, c1 and their trees/blobs) is unioned once rather
	/// than re-materialized per tip — then resolves the result to ids once. The set must still equal the
	/// graph walk (this exercises the OR / already-covered path, distinct from the frontier walk-fill).
	#[tokio::test]
	async fn reachable_from_with_overlapping_bitmapped_tips_matches_the_walk() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();

		// c0 <- c1 <- c2 and c1 <- c3: c2 and c3 both descend from the shared c0/c1.
		let c0 = loose_commit(repo, Vec::new(), "f.txt", b"0").await;
		let c1 = loose_commit(repo, vec![c0], "f.txt", b"1").await;
		let c2 = loose_commit(repo, vec![c1], "f.txt", b"2").await;
		let c3 = loose_commit(repo, vec![c1], "f.txt", b"3").await;
		let roots = vec![c2, c3];

		// Baseline graph walk before any bitmap exists.
		let from_walk = reachable_from(repo, roots.clone()).await.unwrap();

		// Bitmap *both* tips, so both roots hit the OR path (their shared ancestry overlaps).
		repo.objects().repack(u64::MAX).await.unwrap();
		repo
			.objects()
			.write_reachability_bitmap(&[c2, c3])
			.await
			.unwrap();
		assert!(
			repo.objects().has_reachability_bitmap().await.unwrap(),
			"the repo should now have a reachability bitmap",
		);

		let from_bitmap = reachable_from(repo, roots).await.unwrap();

		assert_eq!(
			from_walk, from_bitmap,
			"OR-ing overlapping bitmapped tips must equal the graph walk",
		);
		for id in [c0, c1, c2, c3] {
			assert!(from_bitmap.contains(&id), "commit {id} reachable");
		}
	}

	/// `prune` must never delete a commit listed in `.git/shallow` — even a *stale* entry sitting behind
	/// the current boundary (a repo re-shortened after a deepen) — because the client re-sends it as a
	/// `shallow` line on a later `--unshallow`. Protecting shallow entries as roots keeps them while the
	/// walk still stops at the boundary, so history *past* the boundary is reclaimed.
	#[tokio::test]
	async fn prune_keeps_stale_shallow_entries() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();

		// c0 <- c1 <- c2, all present; `main` at the tip c2.
		let c0 = loose_commit(repo, Vec::new(), "f.txt", b"0").await;
		let c1 = loose_commit(repo, vec![c0], "f.txt", b"1").await;
		let c2 = loose_commit(repo, vec![c1], "f.txt", b"2").await;
		repo
			.refs()
			.update_ref(
				"refs/heads/main",
				c2,
				None,
				gitana_repository::ReflogIntent::Skip,
			)
			.await
			.unwrap();
		// A re-shortened shallow file: the current boundary c2 plus a stale entry c1 behind it.
		repo.write_shallow(&[c1, c2]).await.unwrap();

		prune(&wt).await.unwrap();

		// The tip and *both* shallow entries survive; only history past the boundary (c0) is reclaimed.
		assert!(repo.objects().exists_object(&c2).await.unwrap(), "tip kept");
		assert!(
			repo.objects().exists_object(&c1).await.unwrap(),
			"a stale shallow entry must not be pruned (it is named in .git/shallow)"
		);
		assert!(
			!repo.objects().exists_object(&c0).await.unwrap(),
			"history past the shallow boundary is reclaimed"
		);
	}
}
