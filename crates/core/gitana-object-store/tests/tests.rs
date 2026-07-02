//! Integration tests for packed-object lookup: `write_pack` emits a `.idx` sidecar, reads
//! locate objects through the index (and materialise base + delta objects on demand), a
//! miss is `NotFound`, and reads still succeed when the sidecar is absent.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_file_store_memory::MemoryFileStore;
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, Sha256, TreeEntry, encode_commit, encode_pack,
	encode_tree,
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
		.write_pack(&encode_pack(&objects))
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
		.write_pack(&encode_pack(&sample_graph()))
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
		.write_pack(&encode_pack(&objects))
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
		.write_pack(&encode_pack(&sample_graph()))
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
		.write_pack(&encode_pack(&objects))
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
		.repack()
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
	assert!(store.repack().await.expect("repack again").is_none());
}

#[tokio::test]
async fn repack_of_a_single_pack_is_a_noop() {
	let store = ObjectStore::<_, Sha256>::new(MemoryFileStore::new());
	store
		.write_pack(&encode_pack(&sample_graph()))
		.await
		.expect("write pack");
	assert!(store.repack().await.expect("repack").is_none());
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
		.repack()
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
		.write_pack(&encode_pack(&objects))
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
		.repack()
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
		.write_pack(&encode_pack(&objects))
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
