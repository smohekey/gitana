//! Integration tests for packed-object lookup: `write_pack` emits a `.idx` sidecar, reads
//! locate objects through the index (and materialise base + delta objects on demand), a
//! miss is `NotFound`, and reads still succeed when the sidecar is absent.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_file_store_memory::MemoryFileStore;
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, Sha256, TreeEntry, decode_midx_bitmap,
	decode_multi_pack_index, encode_commit, encode_pack, encode_tree,
};
use gitana_object_store::ObjectStore;

/// A small object graph with two delta-friendly blobs, a tree, and a commit — enough that the
/// encoded pack carries both full and delta entries at several offsets.
fn sample_graph() -> Vec<PackedObject<Sha256>> {
	let mut objects = Vec::new();
	let mut put = |kind: ObjectKind, data: Vec<u8>| {
		let id = ObjectId::<Sha256>::compute(kind, &data);
		objects.push(PackedObject { id, kind, data });
		id
	};

	let body = b"line one\nline two\nline three\n".repeat(40);
	let blob1 = put(ObjectKind::Blob, body.clone());
	let mut body2 = body;
	body2.extend_from_slice(b"line four added later\n");
	put(ObjectKind::Blob, body2);

	let tree = put(
		ObjectKind::Tree,
		encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "file.txt".to_owned(),
			id: blob1,
		}]),
	);
	put(
		ObjectKind::Commit,
		encode_commit(&Commit {
			tree,
			parents: vec![],
			author: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			committer: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "root\n".to_owned(),
		}),
	);

	objects
}

#[tokio::test]
async fn reads_every_object_in_a_stored_pack() {
	let objects = sample_graph();
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");

	for object in &objects {
		let (kind, data) = store
			.read_object(&object.id)
			.await
			.expect("read packed object");
		assert_eq!(kind, object.kind);
		assert_eq!(data, object.data);
		assert!(store.exists_object(&object.id).await.expect("exists"));
	}
}

#[tokio::test]
async fn write_pack_writes_an_idx_sidecar() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&sample_graph()))
		.await
		.expect("write pack");

	let paths = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list");
	assert!(
		paths.iter().any(|p| p.ends_with(".pack")),
		"a .pack is stored: {paths:?}"
	);
	assert!(
		paths.iter().any(|p| p.ends_with(".idx")),
		"an .idx sidecar is stored: {paths:?}"
	);
}

#[tokio::test]
async fn reads_when_the_idx_sidecar_is_missing() {
	// A pack without its `.idx` (a legacy or foreign pack) must still be readable: the store
	// rebuilds the index by decoding the pack once. `write_pack` populates no cache, and we
	// delete the sidecar before the first read, so the read genuinely exercises the fallback.
	let objects = sample_graph();
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");

	let idx_path = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list")
		.into_iter()
		.find(|p| p.ends_with(".idx"))
		.expect("idx present");
	store
		.file_store()
		.delete_path(&idx_path, None)
		.await
		.expect("delete idx");

	for object in &objects {
		let (kind, data) = store
			.read_object(&object.id)
			.await
			.expect("read via fallback");
		assert_eq!(kind, object.kind);
		assert_eq!(data, object.data);
	}
}

#[tokio::test]
async fn an_unknown_object_is_not_found() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&sample_graph()))
		.await
		.expect("write pack");

	let stranger = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"nowhere in the pack");
	assert!(!store.exists_object(&stranger).await.expect("exists"));
	assert!(matches!(
		store.read_object(&stranger).await,
		Err(gitana_object_store::ObjectStoreError::NotFound)
	));
}

/// Whether any loose object (an `objects/<aa>/…` fan-out entry) remains on disk.
async fn has_loose_objects(store: &ObjectStore<MemoryFileStore, Sha256>) -> bool {
	store
		.file_store()
		.list_prefix("objects/")
		.await
		.expect("list objects")
		.iter()
		.any(|entry| {
			let name = entry.rsplit('/').next().unwrap_or_default();
			name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
		})
}

fn pack_count(paths: &[String]) -> usize {
	paths.iter().filter(|p| p.ends_with(".pack")).count()
}

#[tokio::test]
async fn repack_consolidates_loose_and_packed_objects() {
	let objects = sample_graph();
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");
	let loose_a = store
		.write_object(ObjectKind::Blob, b"loose alpha")
		.await
		.expect("write loose a");
	let loose_b = store
		.write_object(ObjectKind::Blob, b"loose beta")
		.await
		.expect("write loose b");

	let report = store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("repack did work");
	assert_eq!(report.packed_objects, objects.len() + 2);
	assert_eq!(report.packs_removed, 1);
	assert_eq!(report.loose_removed, 2);

	// Exactly one pack (+ its .idx) remains, and no loose objects.
	let paths = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list");
	assert_eq!(pack_count(&paths), 1);
	assert_eq!(paths.iter().filter(|p| p.ends_with(".idx")).count(), 1);
	assert!(!has_loose_objects(&store).await);

	// Every object — formerly packed or loose — is still readable, unchanged.
	for object in &objects {
		let (kind, data) = store.read_object(&object.id).await.expect("read");
		assert_eq!(kind, object.kind);
		assert_eq!(data, object.data);
	}
	assert_eq!(
		store.read_object(&loose_a).await.expect("read a").1,
		b"loose alpha"
	);
	assert_eq!(
		store.read_object(&loose_b).await.expect("read b").1,
		b"loose beta"
	);

	// A second repack has nothing to do.
	assert!(
		store
			.repack(u64::MAX)
			.await
			.expect("repack again")
			.is_none()
	);
}

#[tokio::test]
async fn repack_of_a_single_pack_is_a_noop() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&sample_graph()))
		.await
		.expect("write pack");
	assert!(store.repack(u64::MAX).await.expect("repack").is_none());
}

#[tokio::test]
async fn repack_packs_loose_only_stores() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let a = store
		.write_object(ObjectKind::Blob, b"one")
		.await
		.expect("write a");
	let b = store
		.write_object(ObjectKind::Blob, b"two")
		.await
		.expect("write b");

	let report = store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("repack did work");
	assert_eq!(report.packed_objects, 2);
	assert_eq!(report.packs_removed, 0);
	assert_eq!(report.loose_removed, 2);

	let paths = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list");
	assert_eq!(pack_count(&paths), 1);
	assert!(!has_loose_objects(&store).await);
	assert_eq!(store.read_object(&a).await.expect("read a").1, b"one");
	assert_eq!(store.read_object(&b).await.expect("read b").1, b"two");
}

#[tokio::test]
async fn repack_regenerates_a_missing_pack_index() {
	// A single pack with no `.idx` is readable by our fallback but not by stock git, so repack
	// must not treat it as a no-op — it regenerates the sidecar.
	let objects = sample_graph();
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");
	let idx_path = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list")
		.into_iter()
		.find(|p| p.ends_with(".idx"))
		.expect("idx present");
	store
		.file_store()
		.delete_path(&idx_path, None)
		.await
		.expect("delete idx");

	let report = store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("regenerating the index is not a no-op");
	assert_eq!(report.packed_objects, objects.len());

	let paths = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list");
	assert_eq!(pack_count(&paths), 1);
	assert_eq!(paths.iter().filter(|p| p.ends_with(".idx")).count(), 1);
	for object in &objects {
		assert_eq!(
			store.read_object(&object.id).await.expect("read").1,
			object.data
		);
	}
}

#[tokio::test]
async fn prune_loose_deletes_only_objects_absent_from_keep() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let keep_a = store
		.write_object(ObjectKind::Blob, b"keep a")
		.await
		.expect("write keep a");
	let keep_b = store
		.write_object(ObjectKind::Blob, b"keep b")
		.await
		.expect("write keep b");
	let drop_a = store
		.write_object(ObjectKind::Blob, b"drop a")
		.await
		.expect("write drop a");
	let drop_b = store
		.write_object(ObjectKind::Blob, b"drop b")
		.await
		.expect("write drop b");

	let keep: HashSet<ObjectId<Sha256>> = [keep_a, keep_b].into_iter().collect();
	let report = store.prune_loose(&keep).await.expect("prune");
	assert_eq!(report.pruned, 2);

	// Kept objects still read; dropped objects are gone.
	assert_eq!(
		store.read_object(&keep_a).await.expect("keep a").1,
		b"keep a"
	);
	assert_eq!(
		store.read_object(&keep_b).await.expect("keep b").1,
		b"keep b"
	);
	assert!(!store.exists_object(&drop_a).await.expect("exists drop a"));
	assert!(!store.exists_object(&drop_b).await.expect("exists drop b"));

	// Idempotent: with the same keep set, nothing more is pruned.
	assert_eq!(
		store.prune_loose(&keep).await.expect("prune again").pruned,
		0
	);
}

#[tokio::test]
async fn prune_loose_with_empty_keep_removes_all_loose() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	for content in [b"one".as_slice(), b"two", b"three"] {
		store
			.write_object(ObjectKind::Blob, content)
			.await
			.expect("write");
	}
	let report = store.prune_loose(&HashSet::new()).await.expect("prune all");
	assert_eq!(report.pruned, 3);
	assert!(!has_loose_objects(&store).await);
}

#[tokio::test]
async fn prune_loose_leaves_packed_objects_untouched() {
	// prune only deletes loose objects; a packed object absent from `keep` must survive.
	let objects = sample_graph();
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");

	let report = store.prune_loose(&HashSet::new()).await.expect("prune");
	assert_eq!(report.pruned, 0, "no loose objects to prune");
	for object in &objects {
		assert_eq!(
			store.read_object(&object.id).await.expect("read packed").1,
			object.data
		);
	}
}

/// Deterministic, effectively-incompressible bytes (xorshift64), so a blob's packed size stays
/// close to its length and a size limit actually forces a split.
fn incompressible(seed: u64, len: usize) -> Vec<u8> {
	// Mix the seed (bijective ×odd) so distinct seeds yield distinct, non-colliding streams.
	let mut x = seed.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
	let mut out = Vec::with_capacity(len);
	for _ in 0..len {
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		out.push((x & 0xff) as u8);
	}
	out
}

#[tokio::test]
async fn repack_splits_into_size_bounded_packs() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let mut ids = Vec::new();
	let mut contents = Vec::new();
	for seed in 1..=6u64 {
		let data = incompressible(seed, 200 * 1024);
		ids.push(
			store
				.write_object(ObjectKind::Blob, &data)
				.await
				.expect("write blob"),
		);
		contents.push(data);
	}

	let limit = 512 * 1024;
	let report = store
		.repack(limit)
		.await
		.expect("repack")
		.expect("repack did work");
	assert_eq!(report.packed_objects, 6);
	assert!(
		report.packs_written > 1,
		"1.2 MiB of incompressible blobs under a 512 KiB limit must split: got {} pack(s)",
		report.packs_written
	);

	// Every stored pack is within the limit, and every blob still reads back unchanged.
	let packs: Vec<String> = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list")
		.into_iter()
		.filter(|p| p.ends_with(".pack"))
		.collect();
	assert_eq!(packs.len(), report.packs_written);
	for path in &packs {
		let bytes = store.file_store().read_path(path).await.expect("read pack");
		assert!(
			bytes.len() as u64 <= limit,
			"pack {path} is {} bytes, over the {limit} limit",
			bytes.len()
		);
	}
	assert!(!has_loose_objects(&store).await);
	for (id, data) in ids.iter().zip(&contents) {
		assert_eq!(&store.read_object(id).await.expect("read").1, data);
	}

	// Deterministic and idempotent: a second repack re-partitions to exactly the same pack set
	// (a multi-pack repo re-packs so repack can consolidate, but here it is already optimal).
	let before: std::collections::BTreeSet<String> = packs.iter().cloned().collect();
	store.repack(limit).await.expect("repack again");
	let after: std::collections::BTreeSet<String> = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.expect("list")
		.into_iter()
		.filter(|p| p.ends_with(".pack"))
		.collect();
	assert_eq!(before, after, "split repack reproduces the same pack set");
}

#[tokio::test]
async fn repack_with_a_large_limit_makes_one_pack() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	for seed in 1..=4u64 {
		store
			.write_object(ObjectKind::Blob, &incompressible(seed, 100 * 1024))
			.await
			.expect("write blob");
	}
	let report = store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("repack did work");
	assert_eq!(report.packs_written, 1, "a large limit keeps a single pack");
}

#[tokio::test]
async fn repack_splits_an_existing_oversized_pack() {
	// A single already-indexed pack that exceeds a (newly set, smaller) limit must be re-split,
	// not skipped as a no-op.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let objects: Vec<PackedObject<Sha256>> = (1..=6u64)
		.map(|seed| {
			let data = incompressible(seed, 200 * 1024);
			let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
			PackedObject {
				id,
				kind: ObjectKind::Blob,
				data,
			}
		})
		.collect();
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write single pack");
	assert_eq!(
		pack_count(
			&store
				.file_store()
				.list_prefix("objects/pack/")
				.await
				.unwrap()
		),
		1
	);

	let report = store
		.repack(512 * 1024)
		.await
		.expect("repack")
		.expect("an oversized single pack is not a no-op");
	assert!(
		report.packs_written > 1,
		"a single pack over the limit must be split: got {} pack(s)",
		report.packs_written
	);
	for object in &objects {
		assert_eq!(
			store.read_object(&object.id).await.expect("read").1,
			object.data
		);
	}
}

#[tokio::test]
async fn repack_consolidates_multiple_small_packs_into_one() {
	// Several small packs (no loose) that fit within the limit must be consolidated into one, not
	// treated as a no-op just because each is individually within the limit.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let mut all = Vec::new();
	for group in 0..3u64 {
		let objects: Vec<PackedObject<Sha256>> = (0..2u64)
			.map(|i| {
				let data = incompressible(group * 10 + i, 8 * 1024);
				let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
				PackedObject {
					id,
					kind: ObjectKind::Blob,
					data,
				}
			})
			.collect();
		store
			.write_pack(encode_pack(&objects))
			.await
			.expect("write pack");
		all.extend(objects);
	}
	assert_eq!(
		pack_count(
			&store
				.file_store()
				.list_prefix("objects/pack/")
				.await
				.unwrap()
		),
		3,
		"three separate packs to start",
	);

	let report = store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("multiple packs must consolidate, not no-op");
	assert_eq!(report.packs_written, 1, "consolidated into a single pack");
	assert_eq!(report.packs_removed, 3);
	assert_eq!(
		pack_count(
			&store
				.file_store()
				.list_prefix("objects/pack/")
				.await
				.unwrap()
		),
		1,
	);
	for object in &all {
		assert_eq!(
			store.read_object(&object.id).await.expect("read").1,
			object.data
		);
	}
}

const MIDX_PATH: &str = "objects/pack/multi-pack-index";

/// Six incompressible blobs written loose, so a small-limit repack splits them across packs.
async fn loose_blobs(store: &ObjectStore<MemoryFileStore, Sha256>) -> Vec<ObjectId<Sha256>> {
	let mut ids = Vec::new();
	for seed in 1..=6u64 {
		ids.push(
			store
				.write_object(ObjectKind::Blob, &incompressible(seed, 200 * 1024))
				.await
				.expect("write blob"),
		);
	}
	ids
}

#[tokio::test]
async fn repack_writes_then_clears_the_multi_pack_index() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let ids = loose_blobs(&store).await;

	// A split (multiple packs) writes a MIDX; every object still reads (through it).
	let report = store
		.repack(512 * 1024)
		.await
		.expect("repack")
		.expect("work");
	assert!(report.packs_written > 1);
	assert!(
		store.file_store().exists(MIDX_PATH).await.expect("exists"),
		"a multi-pack repack writes a multi-pack-index",
	);
	for id in &ids {
		assert!(store.read_object(id).await.is_ok(), "readable via MIDX");
	}

	// Consolidating back to one pack removes the now-pointless MIDX.
	store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("consolidate");
	assert_eq!(
		pack_count(
			&store
				.file_store()
				.list_prefix("objects/pack/")
				.await
				.unwrap()
		),
		1
	);
	assert!(
		!store.file_store().exists(MIDX_PATH).await.expect("exists"),
		"a single-pack repack clears the MIDX",
	);
	for id in &ids {
		assert!(store.read_object(id).await.is_ok(), "still readable");
	}
}

#[tokio::test]
async fn finds_an_object_in_a_pack_added_after_the_midx() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let midx_ids = loose_blobs(&store).await;
	store
		.repack(512 * 1024)
		.await
		.expect("repack")
		.expect("work");

	// A pack added later is not covered by the MIDX; its objects must still be found (scanned).
	let extra = sample_graph();
	store
		.write_pack(encode_pack(&extra))
		.await
		.expect("write extra pack");

	for object in &extra {
		assert_eq!(
			store
				.read_object(&object.id)
				.await
				.expect("read uncovered")
				.1,
			object.data,
			"an object in a pack added after the MIDX is found",
		);
	}
	// And a MIDX-covered object still reads.
	assert!(store.read_object(&midx_ids[0]).await.is_ok());
}

#[tokio::test]
async fn a_stale_midx_whose_packs_are_gone_is_ignored() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let ids = loose_blobs(&store).await;
	store
		.repack(512 * 1024)
		.await
		.expect("repack")
		.expect("work");

	// Delete every pack (and its .idx) but leave the MIDX, which now names missing packs. A lookup
	// must ignore the stale MIDX and cleanly report `NotFound`, not error on the absent pack.
	for path in store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.unwrap()
	{
		if path.ends_with(".pack") || path.ends_with(".idx") {
			store
				.file_store()
				.delete_path(&path, None)
				.await
				.expect("delete");
		}
	}
	assert!(store.file_store().exists(MIDX_PATH).await.unwrap());

	// A fresh store (no cached MIDX) exercises the stale-load path from scratch.
	assert!(matches!(
		store.read_object(&ids[0]).await,
		Err(gitana_object_store::ObjectStoreError::NotFound)
	));
}

/// A small, distinct, effectively-incompressible blob object for a given seed.
fn blob(seed: u64) -> PackedObject<Sha256> {
	let data = incompressible(seed, 512);
	let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
	PackedObject {
		id,
		kind: ObjectKind::Blob,
		data,
	}
}

fn hex(bytes: &[u8]) -> String {
	let mut s = String::with_capacity(bytes.len() * 2);
	for b in bytes {
		s.push_str(&format!("{b:02x}"));
	}
	s
}

#[tokio::test]
async fn repack_geometric_keeps_the_large_pack() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());

	// One large pack (20 objects) and two small packs (2 each), all disjoint, plus a little loose.
	let big: Vec<PackedObject<Sha256>> = (0..20u64).map(|i| blob(1000 + i)).collect();
	let small_a: Vec<PackedObject<Sha256>> = (0..2u64).map(|i| blob(2000 + i)).collect();
	let small_b: Vec<PackedObject<Sha256>> = (0..2u64).map(|i| blob(3000 + i)).collect();
	let big_pack = encode_pack(&big);
	let big_path = format!(
		"objects/pack/pack-{}.pack",
		hex(&big_pack[big_pack.len() - 32..])
	);
	store.write_pack(big_pack).await.expect("write big");
	store
		.write_pack(encode_pack(&small_a))
		.await
		.expect("write a");
	store
		.write_pack(encode_pack(&small_b))
		.await
		.expect("write b");
	let loose = store
		.write_object(ObjectKind::Blob, &incompressible(4000, 512))
		.await
		.expect("write loose");

	let report = store
		.repack_geometric(u64::MAX, 2)
		.await
		.expect("repack")
		.expect("geometric did work");
	assert_eq!(report.packs_kept, 1, "the large pack is kept in place");

	// The large pack file is untouched (never rewritten), and no loose objects remain.
	assert!(
		store.file_store().exists(&big_path).await.unwrap(),
		"large pack {big_path} kept in place"
	);
	assert!(!has_loose_objects(&store).await);
	// A multi-pack-index covers the kept + new packs.
	assert!(store.file_store().exists(MIDX_PATH).await.unwrap());

	// Every object — kept, rolled up, or formerly loose — still reads back.
	for object in big.iter().chain(&small_a).chain(&small_b) {
		assert_eq!(
			store.read_object(&object.id).await.expect("read").1,
			object.data
		);
	}
	assert!(store.read_object(&loose).await.is_ok());

	// The layout is now geometric (a big pack over a small one), so a second geometric repack is a
	// no-op.
	assert!(
		store
			.repack_geometric(u64::MAX, 2)
			.await
			.expect("repack")
			.is_none(),
		"already geometric",
	);
}

#[tokio::test]
async fn repack_geometric_rewrites_a_kept_pack_missing_its_idx() {
	// A would-be-kept large pack whose `.idx` is gone must not be left in place (stock git needs
	// the sidecar); it is rolled into the batch and rewritten with a fresh `.idx`.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let big: Vec<PackedObject<Sha256>> = (0..20u64).map(|i| blob(1000 + i)).collect();
	let small: Vec<PackedObject<Sha256>> = (0..2u64).map(|i| blob(2000 + i)).collect();
	let big_pack = encode_pack(&big);
	let big_path = format!(
		"objects/pack/pack-{}.pack",
		hex(&big_pack[big_pack.len() - 32..])
	);
	store.write_pack(big_pack).await.expect("write big");
	store
		.write_pack(encode_pack(&small))
		.await
		.expect("write small");

	// Remove the large pack's sidecar so it cannot be kept as-is.
	let big_idx = format!("{}.idx", big_path.strip_suffix(".pack").unwrap());
	store
		.file_store()
		.delete_path(&big_idx, None)
		.await
		.expect("delete big idx");

	store
		.repack_geometric(u64::MAX, 2)
		.await
		.expect("repack")
		.expect("did work");

	// Every remaining pack has its `.idx`, and every object still reads.
	let paths = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.unwrap();
	for pack in paths.iter().filter(|p| p.ends_with(".pack")) {
		let idx = format!("{}.idx", pack.strip_suffix(".pack").unwrap());
		assert!(
			paths.contains(&idx),
			"kept/rewritten pack {pack} has its .idx"
		);
	}
	for object in big.iter().chain(&small) {
		assert_eq!(
			store.read_object(&object.id).await.expect("read").1,
			object.data
		);
	}
}

#[tokio::test]
async fn repack_geometric_folds_loose_weight_into_the_split() {
	// A lone pack is "already geometric" on its own, but enough loose objects beside it must pull it
	// into the batch — otherwise gc would write a second same-sized pack and leave a non-geometric
	// layout. With a 20-object pack and 20 loose, the split rolls both up (nothing kept).
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let packed: Vec<PackedObject<Sha256>> = (0..20u64).map(|i| blob(1000 + i)).collect();
	store.write_pack(encode_pack(&packed)).await.expect("write");
	let mut loose = Vec::new();
	for i in 0..20u64 {
		let data = incompressible(2000 + i, 512);
		loose.push(
			store
				.write_object(ObjectKind::Blob, &data)
				.await
				.expect("loose"),
		);
	}

	let report = store
		.repack_geometric(u64::MAX, 2)
		.await
		.expect("repack")
		.expect("did work");
	assert_eq!(report.packs_kept, 0, "the old pack is rolled in, not kept");

	// The result is now geometric: a second geometric repack is a no-op.
	assert!(
		store
			.repack_geometric(u64::MAX, 2)
			.await
			.expect("repack")
			.is_none(),
		"layout is geometric after folding loose in",
	);
	for object in &packed {
		assert!(store.read_object(&object.id).await.is_ok(), "packed reads");
	}
	for id in &loose {
		assert!(store.read_object(id).await.is_ok(), "loose reads");
	}
}

#[tokio::test]
async fn repack_geometric_rebuilds_a_missing_midx_on_a_noop() {
	// Two packs already in geometric progression (20 and 2 objects) written directly, so no MIDX
	// exists. A geometric repack does no repacking, but the maintenance command must still leave a
	// correct MIDX behind rather than skipping it on the no-op path.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let big: Vec<PackedObject<Sha256>> = (0..20u64).map(|i| blob(1000 + i)).collect();
	let small: Vec<PackedObject<Sha256>> = (0..2u64).map(|i| blob(2000 + i)).collect();
	store.write_pack(encode_pack(&big)).await.expect("big");
	store.write_pack(encode_pack(&small)).await.expect("small");
	assert!(
		!store
			.file_store()
			.exists("objects/pack/multi-pack-index")
			.await
			.unwrap(),
		"no MIDX before repack",
	);

	assert!(
		store
			.repack_geometric(u64::MAX, 2)
			.await
			.expect("repack")
			.is_none(),
		"already geometric — no repack",
	);
	assert!(
		store
			.file_store()
			.exists("objects/pack/multi-pack-index")
			.await
			.unwrap(),
		"MIDX rebuilt on the no-op path",
	);

	// A second call is a true no-op: the MIDX now matches, so it is left untouched.
	assert!(
		store
			.repack_geometric(u64::MAX, 2)
			.await
			.expect("repack")
			.is_none(),
	);
	for object in big.iter().chain(&small) {
		assert!(
			store.read_object(&object.id).await.is_ok(),
			"reads via MIDX"
		);
	}
}

#[tokio::test]
async fn write_reachability_bitmap_is_read_back_by_our_reader() {
	// A tiny history: blob <- tree <- commit c1 <- commit c2 (reusing c1's tree).
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let blob = store
		.write_object(ObjectKind::Blob, b"hello\n")
		.await
		.expect("blob");
	let tree = store
		.write_object(
			ObjectKind::Tree,
			&encode_tree(&[TreeEntry {
				mode: "100644".to_owned(),
				name: "f".to_owned(),
				id: blob,
			}]),
		)
		.await
		.expect("tree");
	let sig = "A U Thor <a@x> 1700000000 +0000".to_owned();
	let c1 = store
		.write_object(
			ObjectKind::Commit,
			&encode_commit(&Commit {
				tree,
				parents: vec![],
				author: sig.clone(),
				committer: sig.clone(),
				signature: None,
				extra_headers: Vec::new(),
				message: "one\n".to_owned(),
			}),
		)
		.await
		.expect("c1");
	let c2 = store
		.write_object(
			ObjectKind::Commit,
			&encode_commit(&Commit {
				tree,
				parents: vec![c1],
				author: sig.clone(),
				committer: sig,
				signature: None,
				extra_headers: Vec::new(),
				message: "two\n".to_owned(),
			}),
		)
		.await
		.expect("c2");

	// Pack everything, then write the MIDX + reachability bitmap over the two commits.
	store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("did work");
	let report = store
		.write_reachability_bitmap(&[c1, c2])
		.await
		.expect("write bitmap")
		.expect("had packs");
	assert_eq!(report.packs, 1);
	assert_eq!(report.bitmapped_commits, 2);

	// The MIDX now carries a reverse index, and the bitmap it names reads back through our reader.
	let midx_bytes = store
		.file_store()
		.read_path("objects/pack/multi-pack-index")
		.await
		.expect("midx");
	let midx = decode_multi_pack_index::<Sha256>(&midx_bytes).expect("decode midx");
	assert!(midx.reverse_index().is_some());

	let bitmap_path = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.unwrap()
		.into_iter()
		.find(|p| p.ends_with(".bitmap"))
		.expect("a .bitmap was written");
	let bitmap_bytes = store
		.file_store()
		.read_path(&bitmap_path)
		.await
		.expect("bitmap");
	let index = decode_midx_bitmap::<Sha256>(&bitmap_bytes).expect("decode bitmap");
	assert_eq!(index.midx_checksum(), midx.checksum());

	let reachable = |commit: &ObjectId<Sha256>| -> HashSet<ObjectId<Sha256>> {
		index
			.reachable_from(commit, &midx)
			.expect("reachable")
			.into_iter()
			.collect()
	};
	assert_eq!(reachable(&c1), HashSet::from([c1, tree, blob]));
	assert_eq!(reachable(&c2), HashSet::from([c2, c1, tree, blob]));

	// A second call replaces the bitmap cleanly (still exactly one).
	store
		.write_reachability_bitmap(&[c2])
		.await
		.expect("rewrite")
		.expect("had packs");
	let count_bitmaps = || async {
		store
			.file_store()
			.list_prefix("objects/pack/")
			.await
			.unwrap()
			.into_iter()
			.filter(|p| p.ends_with(".bitmap"))
			.count()
	};
	assert_eq!(count_bitmaps().await, 1, "exactly one bitmap remains");

	// A plain repack rewrites the MIDX and must not leave the now-stale bitmap behind.
	store
		.write_object(ObjectKind::Blob, b"more\n")
		.await
		.expect("loose");
	store
		.repack(u64::MAX)
		.await
		.expect("repack")
		.expect("did work");
	assert_eq!(
		count_bitmaps().await,
		0,
		"a plain repack clears the stale bitmap",
	);
}

#[tokio::test]
async fn write_reachability_bitmap_skips_a_loose_selected_commit() {
	// c1 is packed; c2 is a still-loose ref tip. Bitmapping must cover c1 and skip c2, not fail.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let blob = store.write_object(ObjectKind::Blob, b"x\n").await.unwrap();
	let tree = store
		.write_object(
			ObjectKind::Tree,
			&encode_tree(&[TreeEntry {
				mode: "100644".to_owned(),
				name: "f".to_owned(),
				id: blob,
			}]),
		)
		.await
		.unwrap();
	let sig = "A U Thor <a@x> 1700000000 +0000".to_owned();
	let c1 = store
		.write_object(
			ObjectKind::Commit,
			&encode_commit(&Commit {
				tree,
				parents: vec![],
				author: sig.clone(),
				committer: sig.clone(),
				signature: None,
				extra_headers: Vec::new(),
				message: "one\n".to_owned(),
			}),
		)
		.await
		.unwrap();
	store.repack(u64::MAX).await.unwrap().expect("packed c1");

	// c2 stays loose (written after the repack), so it is not in the MIDX.
	let c2 = store
		.write_object(
			ObjectKind::Commit,
			&encode_commit(&Commit {
				tree,
				parents: vec![c1],
				author: sig.clone(),
				committer: sig,
				signature: None,
				extra_headers: Vec::new(),
				message: "two\n".to_owned(),
			}),
		)
		.await
		.unwrap();

	let report = store
		.write_reachability_bitmap(&[c1, c2])
		.await
		.expect("write bitmap")
		.expect("had packs");
	assert_eq!(
		report.bitmapped_commits, 1,
		"only the packed commit is bitmapped"
	);
}

/// The longest lowercase-hex prefix shared by any two of `ids`, and the two ids sharing it.
/// Adjacent ids in sorted (== hex) order have the longest common prefix, so one scan suffices.
fn longest_shared_hex_prefix(
	ids: &[ObjectId<Sha256>],
) -> (String, ObjectId<Sha256>, ObjectId<Sha256>) {
	let mut sorted = ids.to_vec();
	sorted.sort();
	let mut best = (String::new(), sorted[0], sorted[0]);
	for pair in sorted.windows(2) {
		let (a, b) = (pair[0].to_hex(), pair[1].to_hex());
		let shared: String = a
			.chars()
			.zip(b.chars())
			.take_while(|(x, y)| x == y)
			.map(|(x, _)| x)
			.collect();
		if shared.len() > best.0.len() {
			best = (shared, pair[0], pair[1]);
		}
	}
	best
}

#[tokio::test]
async fn find_by_prefix_finds_loose_and_packed_objects() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let packed = sample_graph();
	store
		.write_pack(encode_pack(&packed))
		.await
		.expect("write pack");
	let loose_a = store
		.write_object(ObjectKind::Blob, b"loose alpha")
		.await
		.expect("write loose a");
	let loose_b = store
		.write_object(ObjectKind::Blob, b"loose beta")
		.await
		.expect("write loose b");

	// A 12-hex-char (48-bit) prefix resolves each object uniquely, whether it is loose or packed.
	let mut ids: Vec<ObjectId<Sha256>> = packed.iter().map(|o| o.id).collect();
	ids.push(loose_a);
	ids.push(loose_b);
	for id in ids {
		assert_eq!(
			store
				.find_by_prefix(&id.to_hex()[..12])
				.await
				.expect("find"),
			vec![id],
			"unique prefix of {id} must resolve to just it",
		);
	}
}

#[tokio::test]
async fn find_by_prefix_dedups_an_object_that_is_both_loose_and_packed() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let data = b"stored twice".to_vec();
	let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);

	// Write it loose, then also inside a pack — the same id now lives in both storages.
	store
		.write_object(ObjectKind::Blob, &data)
		.await
		.expect("write loose");
	store
		.write_pack(encode_pack(&[PackedObject {
			id,
			kind: ObjectKind::Blob,
			data,
		}]))
		.await
		.expect("write pack");

	assert_eq!(
		store
			.find_by_prefix(&id.to_hex()[..12])
			.await
			.expect("find"),
		vec![id],
		"an object stored both loose and packed is reported once, not as ambiguous",
	);
}

#[tokio::test]
async fn find_by_prefix_spans_a_multi_pack_index() {
	// 1.2 MiB of incompressible blobs under a 512 KiB limit splits into several packs, which a
	// repack covers with a multi-pack-index. Prefix resolution must consult it.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let mut ids = Vec::new();
	for seed in 1..=6u64 {
		ids.push(
			store
				.write_object(ObjectKind::Blob, &incompressible(seed, 200 * 1024))
				.await
				.expect("write blob"),
		);
	}
	let report = store
		.repack(512 * 1024)
		.await
		.expect("repack")
		.expect("repack did work");
	assert!(report.packs_written > 1, "fixture must split into >1 pack");
	assert!(
		store
			.file_store()
			.exists("objects/pack/multi-pack-index")
			.await
			.expect("stat midx"),
		"a multi-pack repo carries a MIDX",
	);
	assert!(
		!has_loose_objects(&store).await,
		"repack removed loose objects"
	);

	for id in ids {
		assert_eq!(
			store
				.find_by_prefix(&id.to_hex()[..16])
				.await
				.expect("find"),
			vec![id],
			"packed object {id} must resolve by prefix through the MIDX",
		);
	}
}

#[tokio::test]
async fn find_by_prefix_reports_every_ambiguous_match() {
	// Pack enough blobs that two share a hex prefix, then confirm the shared prefix resolves to
	// both (the caller reads more-than-one as ambiguous) while a longer prefix disambiguates.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let objects: Vec<PackedObject<Sha256>> = (0..64u64)
		.map(|seed| {
			let data = incompressible(seed, 64);
			let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
			PackedObject {
				id,
				kind: ObjectKind::Blob,
				data,
			}
		})
		.collect();
	store
		.write_pack(encode_pack(&objects))
		.await
		.expect("write pack");

	let ids: Vec<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
	// Pigeonhole: 64 ids over 16 first-hex-char buckets guarantees a shared prefix of length ≥ 1.
	let (prefix, a, b) = longest_shared_hex_prefix(&ids);
	assert!(!prefix.is_empty(), "fixture must have a shared prefix");

	let matches = store.find_by_prefix(&prefix).await.expect("find");
	assert!(
		matches.contains(&a) && matches.contains(&b) && matches.len() >= 2,
		"shared prefix {prefix:?} must resolve ambiguously: {matches:?}",
	);

	// One char past the divergence point, `a` still matches but `b` no longer does.
	let longer = &a.to_hex()[..prefix.len() + 1];
	let matches = store.find_by_prefix(longer).await.expect("find");
	assert!(matches.contains(&a) && !matches.contains(&b));
}

/// A packed linear history blob <- tree <- c1 <- c2 (c2 reuses c1's tree), returned as
/// (blob, tree, c1, c2). Everything is repacked so a bitmap can be written over it.
async fn packed_history(
	store: &ObjectStore<MemoryFileStore, Sha256>,
) -> (
	ObjectId<Sha256>,
	ObjectId<Sha256>,
	ObjectId<Sha256>,
	ObjectId<Sha256>,
) {
	let blob = store.write_object(ObjectKind::Blob, b"hi\n").await.unwrap();
	let tree = store
		.write_object(
			ObjectKind::Tree,
			&encode_tree(&[TreeEntry {
				mode: "100644".to_owned(),
				name: "f".to_owned(),
				id: blob,
			}]),
		)
		.await
		.unwrap();
	let sig = "A U Thor <a@x> 1700000000 +0000".to_owned();
	let mk = |parents: Vec<ObjectId<Sha256>>, message: &str| {
		encode_commit(&Commit {
			tree,
			parents,
			author: sig.clone(),
			committer: sig.clone(),
			signature: None,
			extra_headers: Vec::new(),
			message: message.to_owned(),
		})
	};
	let c1 = store
		.write_object(ObjectKind::Commit, &mk(vec![], "one\n"))
		.await
		.unwrap();
	let c2 = store
		.write_object(ObjectKind::Commit, &mk(vec![c1], "two\n"))
		.await
		.unwrap();
	store.repack(u64::MAX).await.expect("repack").expect("work");
	(blob, tree, c1, c2)
}

#[tokio::test]
async fn reachable_object_closure_bulk_adds_a_bitmapped_commit() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (blob, tree, c1, c2) = packed_history(&store).await;
	store
		.write_reachability_bitmap(&[c1, c2])
		.await
		.expect("bitmap")
		.expect("packs");

	let closure = store.reachable_object_closure(&[c2], true).await.unwrap();
	assert_eq!(closure, HashSet::from([c2, c1, tree, blob]));
}

#[tokio::test]
async fn reachable_object_closure_walks_down_to_a_bitmapped_ancestor() {
	// Bitmap only c1; c2 is packed but unbitmapped, so the closure must walk c2 and then bulk-add the
	// bitmapped c1's closure — the fill-in path.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (blob, tree, c1, c2) = packed_history(&store).await;
	store
		.write_reachability_bitmap(&[c1])
		.await
		.expect("bitmap")
		.expect("packs");

	let closure = store.reachable_object_closure(&[c2], true).await.unwrap();
	assert_eq!(closure, HashSet::from([c2, c1, tree, blob]));
}

#[tokio::test]
async fn reachable_object_closure_without_a_bitmap_walks_the_graph() {
	// No bitmap written: the closure is a plain walk and must still enumerate the whole history.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (blob, tree, c1, c2) = packed_history(&store).await;
	assert!(!store.has_reachability_bitmap().await.unwrap());

	let closure = store.reachable_object_closure(&[c2], true).await.unwrap();
	assert_eq!(closure, HashSet::from([c2, c1, tree, blob]));
}

#[tokio::test]
async fn reachable_object_closure_strict_errors_but_lenient_skips_a_missing_tip() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let stranger = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"absent");
	// Lenient (the have side): an object the store lacks is skipped, leaving an empty closure.
	assert!(
		store
			.reachable_object_closure(&[stranger], false)
			.await
			.unwrap()
			.is_empty()
	);
	// Strict (the want side): a missing object is a connectivity error.
	assert!(
		store
			.reachable_object_closure(&[stranger], true)
			.await
			.is_err()
	);
}

#[tokio::test]
async fn a_bitmap_over_a_stale_midx_is_not_trusted() {
	// A MIDX (and its checksum-matched bitmap) can outlive a pack it names if a pack is removed
	// out-of-band. Object lookup treats such a MIDX as stale and scans packs directly; the bitmap
	// bound to it must be distrusted the same way, so the pack builder falls back to the walk.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (_blob, _tree, c1, c2) = packed_history(&store).await;
	store
		.write_reachability_bitmap(&[c1, c2])
		.await
		.expect("bitmap")
		.expect("packs");
	assert!(store.has_reachability_bitmap().await.unwrap());

	// Remove the pack the MIDX names, without rewriting the MIDX — the stale-index scenario.
	let pack = store
		.file_store()
		.list_prefix("objects/pack/")
		.await
		.unwrap()
		.into_iter()
		.find(|p| p.ends_with(".pack"))
		.expect("a pack");
	store
		.file_store()
		.delete_path(&pack, None)
		.await
		.expect("delete pack");

	assert!(
		!store.has_reachability_bitmap().await.unwrap(),
		"a bitmap over a MIDX naming a deleted pack must not be trusted",
	);
}

#[tokio::test]
async fn commit_reaches_any_is_commit_only() {
	// The commit-only query resolves ancestry among commits and never reports a reachable tree or blob
	// as a target (they are reachable objects, but not commits).
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (blob, tree, c1, c2) = packed_history(&store).await;
	store
		.write_reachability_bitmap(&[c2])
		.await
		.expect("bitmap")
		.expect("packs");
	assert!(
		store
			.commit_reaches_any(c2, &HashSet::from([c1]))
			.await
			.unwrap()
	);
	assert!(
		!store
			.commit_reaches_any(c1, &HashSet::from([c2]))
			.await
			.unwrap()
	);
	// The tree and blob are reachable from c2 as objects, but are not commits, so ancestry ignores them.
	assert!(
		!store
			.commit_reaches_any(c2, &HashSet::from([tree, blob]))
			.await
			.unwrap()
	);
}

#[tokio::test]
async fn commit_reaches_any_walks_down_to_a_bitmapped_ancestor() {
	// Bitmap only c1; c2 is packed but unbitmapped, so the query walks c2's parents to the bitmapped c1
	// and tests it — the fill-in path.
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (_blob, tree, c1, c2) = packed_history(&store).await;
	store
		.write_reachability_bitmap(&[c1])
		.await
		.expect("bitmap")
		.expect("packs");
	assert!(
		store
			.commit_reaches_any(c2, &HashSet::from([c1]))
			.await
			.unwrap()
	);
	assert!(
		!store
			.commit_reaches_any(c2, &HashSet::from([tree]))
			.await
			.unwrap()
	);
}

#[tokio::test]
async fn commit_reaches_any_without_a_bitmap_walks() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	let (_blob, _tree, c1, c2) = packed_history(&store).await;
	assert!(!store.has_reachability_bitmap().await.unwrap());
	assert!(
		store
			.commit_reaches_any(c2, &HashSet::from([c1]))
			.await
			.unwrap()
	);
	assert!(
		!store
			.commit_reaches_any(c1, &HashSet::from([c2]))
			.await
			.unwrap()
	);
	// A commit trivially reaches itself.
	assert!(
		store
			.commit_reaches_any(c2, &HashSet::from([c2]))
			.await
			.unwrap()
	);
}
