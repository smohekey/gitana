//! The reachability-bitmap fast path in the pack builder must produce exactly the same object set as
//! the graph walk it replaces — across a full clone, an incremental fetch, an un-bitmapped tip
//! (walk-fill), and a thin pack. Each test bitmaps one repo (repack, then write the bitmap over its
//! tips) and compares its pack against an identical un-bitmapped repo. Object ids are
//! content-addressed, so the two repos share ids and the only difference is which enumeration path
//! runs — guarded by asserting the bitmapped repo actually has a bitmap.

use std::collections::{HashMap, HashSet};

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{build_pack, build_pack_thin};
use gitana_object::{
	ObjectId, PackedObject, Sha256, decode_pack, decode_pack_with_bases, ref_delta_base_ids,
};
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};

fn repo() -> Repository<MemoryFileStore, Sha256> {
	Repository::new(ObjectStore::<_, Sha256>::new(MemoryFileStore::new()))
}

/// Add commit `i` on `main`: a new distinct file appended to `entries`, committed on `HEAD`.
/// Deterministic (fixed identity, timestamp, content), so the same `i` yields the same object ids.
async fn add_commit(
	repo: &Repository<MemoryFileStore, Sha256>,
	entries: &mut Vec<TreeBuildEntry<Sha256>>,
	i: usize,
) -> ObjectId<Sha256> {
	let blob = repo
		.write_blob(format!("content {i}\n").as_bytes())
		.await
		.expect("blob");
	entries.push(TreeBuildEntry {
		path: format!("file{i}.txt"),
		mode: FileMode::Regular,
		id: blob,
	});
	let tree = repo.write_tree(entries).await.expect("tree");
	let who = "A U Thor <a@x> 1700000000 +0000";
	repo
		.commit_on_head(tree, who, who, &format!("commit {i}\n"))
		.await
		.expect("commit")
}

/// A fresh repo with a linear history of `n` commits on `main`, returned oldest-first.
async fn chain(n: usize) -> (Repository<MemoryFileStore, Sha256>, Vec<ObjectId<Sha256>>) {
	let repo = repo();
	repo.init().await.expect("init");
	let mut entries = Vec::new();
	let mut commits = Vec::new();
	for i in 0..n {
		commits.push(add_commit(&repo, &mut entries, i).await);
	}
	(repo, commits)
}

/// Repack then bitmap `tips`, so the pack builder takes the bitmap fast path. Asserts the bitmap
/// really landed, so an equivalence test cannot pass by silently running the walk on both sides.
async fn bitmap(repo: &Repository<MemoryFileStore, Sha256>, tips: &[ObjectId<Sha256>]) {
	repo.objects().repack(u64::MAX).await.expect("repack");
	repo
		.objects()
		.write_reachability_bitmap(tips)
		.await
		.expect("write bitmap");
	assert!(
		repo
			.objects()
			.has_reachability_bitmap()
			.await
			.expect("bitmap?"),
		"the repo should now have a reachability bitmap",
	);
}

fn ids(objects: &[PackedObject<Sha256>]) -> HashSet<ObjectId<Sha256>> {
	objects.iter().map(|o| o.id).collect()
}

/// Complete a (possibly thin) pack against the repo's object store, then return its object ids.
async fn completed_ids(
	repo: &Repository<MemoryFileStore, Sha256>,
	pack: &[u8],
) -> HashSet<ObjectId<Sha256>> {
	let mut bases = HashMap::new();
	for id in ref_delta_base_ids::<Sha256>(pack).expect("scan bases") {
		if let Ok((kind, data)) = repo.objects().read_object(&id).await {
			bases.insert(id, (kind, data));
		}
	}
	ids(&decode_pack_with_bases::<Sha256>(pack, &bases).expect("complete"))
}

#[tokio::test]
async fn bitmap_full_clone_matches_the_walk() {
	let (bmp, commits) = chain(3).await;
	bitmap(&bmp, &[commits[2]]).await;

	let (walked, walked_commits) = chain(3).await;
	assert_eq!(commits, walked_commits, "identical content ⇒ identical ids");
	assert!(
		!walked
			.objects()
			.has_reachability_bitmap()
			.await
			.expect("bitmap?"),
		"the comparison repo must run the walk, not a bitmap",
	);

	let tip = commits[2];
	let from_bitmap =
		ids(&decode_pack::<Sha256>(&build_pack(&bmp, &[tip], &[]).await.unwrap()).unwrap());
	let from_walk =
		ids(&decode_pack::<Sha256>(&build_pack(&walked, &[tip], &[]).await.unwrap()).unwrap());
	assert_eq!(from_bitmap, from_walk);
	assert!(!from_bitmap.is_empty());
	assert!(from_bitmap.contains(&tip));
}

#[tokio::test]
async fn bitmap_incremental_fetch_matches_the_walk() {
	let (bmp, commits) = chain(4).await;
	bitmap(&bmp, &[commits[3]]).await;

	let (walked, walked_commits) = chain(4).await;
	assert_eq!(commits, walked_commits);

	let (want, have) = (commits[3], commits[1]);
	let from_bitmap =
		ids(&decode_pack::<Sha256>(&build_pack(&bmp, &[want], &[have]).await.unwrap()).unwrap());
	let from_walk =
		ids(&decode_pack::<Sha256>(&build_pack(&walked, &[want], &[have]).await.unwrap()).unwrap());
	assert_eq!(from_bitmap, from_walk);
	// The `have` cuts the shared prefix: the have and everything older are excluded, the new tip kept.
	assert!(!from_bitmap.contains(&commits[0]));
	assert!(!from_bitmap.contains(&commits[1]));
	assert!(from_bitmap.contains(&commits[3]));
}

#[tokio::test]
async fn bitmap_walk_fill_covers_an_unbitmapped_tip() {
	// Bitmap c0..c1, then add c2 loose *after* bitmapping: c2 is unbitmapped, so the builder must walk
	// it down to the bitmapped c1 and union that closure in (git's fill-in).
	let bmp = repo();
	bmp.init().await.expect("init");
	let mut entries = Vec::new();
	let c0 = add_commit(&bmp, &mut entries, 0).await;
	let c1 = add_commit(&bmp, &mut entries, 1).await;
	bitmap(&bmp, &[c1]).await;
	let c2 = add_commit(&bmp, &mut entries, 2).await;

	let (walked, walked_commits) = chain(3).await;
	assert_eq!(vec![c0, c1, c2], walked_commits);

	let from_bitmap =
		ids(&decode_pack::<Sha256>(&build_pack(&bmp, &[c2], &[]).await.unwrap()).unwrap());
	let from_walk =
		ids(&decode_pack::<Sha256>(&build_pack(&walked, &[c2], &[]).await.unwrap()).unwrap());
	assert_eq!(from_bitmap, from_walk);
	assert!(from_bitmap.contains(&c2));
	assert!(from_bitmap.contains(&c0));
}

#[tokio::test]
async fn bitmap_thin_pack_matches_the_walk() {
	let (bmp, commits) = chain(3).await;
	bitmap(&bmp, &[commits[2]]).await;

	let (walked, walked_commits) = chain(3).await;
	assert_eq!(commits, walked_commits);

	let (want, have) = (commits[2], commits[0]);
	// A thin pack may deltify against bases the peer already has; completing it against the store
	// resolves those, so the two paths must complete to the same object set.
	let thin_bitmap = build_pack_thin(&bmp, &[want], &[have]).await.unwrap();
	let thin_walk = build_pack_thin(&walked, &[want], &[have]).await.unwrap();
	let set_bitmap = completed_ids(&bmp, &thin_bitmap).await;
	let set_walk = completed_ids(&walked, &thin_walk).await;
	assert_eq!(set_bitmap, set_walk);
	assert!(set_bitmap.contains(&want));
	assert!(!set_bitmap.contains(&have));
}
