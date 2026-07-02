//! Integration tests for packed-object lookup: `write_pack` emits a `.idx` sidecar, reads
//! locate objects through the index (and materialise base + delta objects on demand), a
//! miss is `NotFound`, and reads still succeed when the sidecar is absent.

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
