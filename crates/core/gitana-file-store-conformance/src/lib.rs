//! Reusable conformance suite for [`FileStore`] backends.
//!
//! Each backend crate calls [`check_file_store`] from its own `tests/` so every
//! backend is held to the same contract.

use gitana_file_store::{DeleteOutcome, FileStore, FileStoreError, WriteOutcome};

/// Run the full [`FileStore`] contract against `store`. Panics on any violation.
pub async fn check_file_store<S: FileStore>(store: &S) {
	check_immutable_writes(store).await;
	check_shared_handle(store).await;
	check_path_locks(store).await;
	check_cas_writes(store).await;
	check_deletes(store).await;
	check_unlocked_deletes(store).await;
	check_is_dir(store).await;
	check_listing(store).await;
	check_streaming(store).await;
	store
		.durability_barrier()
		.await
		.expect("durability barrier");
}

async fn check_shared_handle(store: &impl FileStore) {
	let shared = store.shared_handle();
	let state = "refs/heads/shared-handle-state";
	assert_eq!(
		shared
			.write_path_if_absent(state, b"one")
			.await
			.expect("write through shared handle"),
		WriteOutcome::Written,
	);
	assert_eq!(
		store
			.read_path(state)
			.await
			.expect("read through original handle"),
		b"one",
		"a shared handle must alias the original backend",
	);
	let (_, version) = store
		.read_path_versioned(state)
		.await
		.expect("read shared version through original handle");
	shared
		.write_path_cas(state, b"two", Some(&version))
		.await
		.expect("CAS through shared handle");
	assert_eq!(
		store
			.read_path(state)
			.await
			.expect("read shared CAS result"),
		b"two",
		"shared handles must observe one version sequence",
	);

	let path = "refs/heads/shared-handle.lock";
	let held = store
		.try_lock_path(path)
		.await
		.expect("lock through original handle")
		.expect("an absent path must be lockable");
	assert!(
		shared
			.try_lock_path(path)
			.await
			.expect("lock through shared handle")
			.is_none(),
		"a shared handle must observe the original handle's lock",
	);
	drop(held);
	let reacquired = shared
		.try_lock_path(path)
		.await
		.expect("lock through shared handle after release");
	assert!(
		reacquired.is_some(),
		"a shared handle must observe release through the original handle",
	);
	drop(reacquired);
}

async fn check_path_locks(store: &impl FileStore) {
	let path = "refs/heads/path-lock.lock";
	let held = store
		.try_lock_path(path)
		.await
		.expect("first lock attempt")
		.expect("an absent path must be lockable");
	assert!(store.exists(path).await.expect("held lock exists"));
	assert!(
		store
			.try_lock_path(path)
			.await
			.expect("contended lock attempt")
			.is_none(),
		"an existing lock path must report contention",
	);
	drop(held);
	assert!(
		!store.exists(path).await.expect("dropped lock is absent"),
		"dropping a path lock must release it synchronously",
	);
	assert!(
		store
			.try_lock_path(path)
			.await
			.expect("lock after release")
			.is_some(),
		"a released path must be lockable again",
	);
}

async fn check_streaming(store: &impl FileStore) {
	use tokio::io::AsyncReadExt;

	// Larger than one 64 KiB chunk, so the chunked write/read paths are exercised.
	let path = "objects/pack/streamed";
	let data = vec![0x5a_u8; 200_000];

	assert_eq!(
		store
			.write_path_stream_if_absent(
				path,
				Box::new(std::io::Cursor::new(data.clone())),
				1_000_000
			)
			.await
			.expect("stream write"),
		WriteOutcome::Written,
	);

	// Immutable: a second stream write to the same path does not overwrite.
	assert_eq!(
		store
			.write_path_stream_if_absent(
				path,
				Box::new(std::io::Cursor::new(data.clone())),
				1_000_000
			)
			.await
			.expect("second stream write"),
		WriteOutcome::AlreadyExists,
	);

	let mut got = Vec::new();
	store
		.read_path_stream(path)
		.await
		.expect("stream read")
		.read_to_end(&mut got)
		.await
		.expect("drain stream");
	assert_eq!(got, data, "streamed read must return what was written");

	assert_eq!(
		store.size(path).await.expect("size"),
		data.len() as u64,
		"size must report the byte length",
	);
	assert!(
		matches!(
			store.size("no/such/path").await,
			Err(FileStoreError::NotFound)
		),
		"size of a missing path is NotFound",
	);

	assert_eq!(
		store
			.read_path_range(path, 100, 50)
			.await
			.expect("range read"),
		data[100..150],
	);

	// A range past the end is clamped, not an error.
	let tail = store
		.read_path_range(path, data.len() as u64 - 10, 1000)
		.await
		.expect("clamped range");
	assert_eq!(tail.len(), 10);

	// Exceeding the cap is rejected.
	assert!(
		matches!(
			store
				.write_path_stream_if_absent(
					"objects/pack/too-big",
					Box::new(std::io::Cursor::new(vec![0u8; 100])),
					10,
				)
				.await,
			Err(FileStoreError::TooLarge { .. })
		),
		"a stream over the cap must be rejected",
	);
}

async fn check_listing(store: &impl FileStore) {
	store
		.write_path_if_absent("objects/pack/pack-a.pack", b"a")
		.await
		.expect("write pack a");
	store
		.write_path_if_absent("objects/pack/pack-b.pack", b"b")
		.await
		.expect("write pack b");
	// Directly under objects/, not objects/pack/ — must not be listed.
	store
		.write_path_if_absent("objects/loose-marker", b"c")
		.await
		.expect("write loose marker");

	let mut got = store
		.list_prefix("objects/pack/")
		.await
		.expect("list pack dir");
	got.sort();
	assert_eq!(
		got,
		vec![
			"objects/pack/pack-a.pack".to_owned(),
			"objects/pack/pack-b.pack".to_owned(),
		],
		"list_prefix must return exactly the entries directly under the prefix dir"
	);
}

async fn check_immutable_writes(store: &impl FileStore) {
	let head = "HEAD";

	assert!(
		matches!(store.read_path(head).await, Err(FileStoreError::NotFound)),
		"reading an absent path must return NotFound"
	);
	assert!(
		!store.exists(head).await.expect("exists must not error"),
		"an unwritten path must not exist"
	);

	assert_eq!(
		store
			.write_path_if_absent(head, b"ref: refs/heads/main\n")
			.await
			.expect("first write must succeed"),
		WriteOutcome::Written,
	);
	assert!(
		store.exists(head).await.expect("exists must not error"),
		"a written path must exist"
	);
	assert_eq!(
		store
			.read_path(head)
			.await
			.expect("written path must read back"),
		b"ref: refs/heads/main\n",
	);
	assert_eq!(
		store
			.write_path_if_absent(head, b"other")
			.await
			.expect("a second if-absent write must not error"),
		WriteOutcome::AlreadyExists,
		"write_path_if_absent must refuse an existing path",
	);
}

async fn check_cas_writes(store: &impl FileStore) {
	let main = "refs/heads/main";

	// Create with expected == None (path absent).
	let v0 = store
		.write_path_cas(main, b"commit-a", None)
		.await
		.expect("CAS create on an absent path must succeed");

	let (bytes, read_version) = store
		.read_path_versioned(main)
		.await
		.expect("read_versioned must succeed");
	assert_eq!(bytes, b"commit-a");
	assert_eq!(
		read_version, v0,
		"read version must equal the create version"
	);

	// Update with the matching version.
	let v1 = store
		.write_path_cas(main, b"commit-b", Some(&v0))
		.await
		.expect("CAS update with the matching version must succeed");
	assert_eq!(store.read_path(main).await.expect("read back"), b"commit-b");

	// Update with a stale version must fail.
	assert!(
		matches!(
			store.write_path_cas(main, b"commit-c", Some(&v0)).await,
			Err(FileStoreError::VersionMismatch)
		),
		"CAS with a stale version must fail",
	);

	// Create-form (expected == None) on an existing path must fail.
	assert!(
		matches!(
			store.write_path_cas(main, b"commit-d", None).await,
			Err(FileStoreError::VersionMismatch)
		),
		"CAS create on an existing path must fail",
	);

	let _ = v1;
}

async fn check_deletes(store: &impl FileStore) {
	let temp = "refs/heads/temp";

	let version = store
		.write_path_cas(temp, b"x", None)
		.await
		.expect("create for delete test");

	// Wrong version must not delete.
	let wrong = gitana_file_store::Version("definitely-not-the-version".into());
	assert!(
		matches!(
			store.delete_path(temp, Some(&wrong)).await,
			Err(FileStoreError::VersionMismatch)
		),
		"delete with a wrong version must fail",
	);

	assert_eq!(
		store
			.delete_path(temp, Some(&version))
			.await
			.expect("delete with the matching version must succeed"),
		DeleteOutcome::Deleted,
	);
	assert_eq!(
		store
			.delete_path(temp, None)
			.await
			.expect("delete of an absent path must not error"),
		DeleteOutcome::NotFound,
	);
}

/// [`FileStore::is_dir`] reports `false` for a regular value and for an absent path (both backends
/// agree on these; a backend with real directories additionally reports `true` for one, covered by
/// its own tests).
async fn check_is_dir(store: &impl FileStore) {
	store
		.write_path_replace("refs/heads/isdir", b"x")
		.await
		.expect("create for is_dir test");
	assert!(
		!store.is_dir("refs/heads/isdir").await.expect("is_dir file"),
		"a value is not a directory",
	);
	assert!(
		!store
			.is_dir("refs/heads/absent")
			.await
			.expect("is_dir absent"),
		"an absent path is not a directory",
	);
	// Removing a non-existent directory errors (both backends agree; a best-effort pruner treats it
	// as "stop"). Removing an actual empty directory is backend-specific and covered by those tests.
	assert!(
		store.remove_dir("refs/heads/absent").await.is_err(),
		"removing an absent directory must error",
	);
}

/// [`FileStore::delete_path_unlocked`] removes unconditionally (no version check) and reports
/// `Deleted` / `NotFound`, and — crucially — does not deadlock when the caller already holds the
/// path's `<path>.lock` (the reason it exists).
async fn check_unlocked_deletes(store: &impl FileStore) {
	let temp = "refs/heads/unlocked-temp";

	store
		.write_path_cas(temp, b"x", None)
		.await
		.expect("create for unlocked delete test");

	// Take the path's own `<path>.lock` (as a ref transaction does mid-commit), then delete: the
	// locked variant would spin and fail here, so this proves the unlocked variant does not contend.
	let lock = format!("{temp}.lock");
	assert_eq!(
		store
			.write_path_if_absent(&lock, b"")
			.await
			.expect("acquire the path's lock"),
		WriteOutcome::Written,
	);
	assert_eq!(
		store
			.delete_path_unlocked(temp)
			.await
			.expect("unlocked delete while the path lock is held must succeed"),
		DeleteOutcome::Deleted,
	);
	assert_eq!(
		store
			.delete_path_unlocked(temp)
			.await
			.expect("unlocked delete of an absent path must not error"),
		DeleteOutcome::NotFound,
	);
	store
		.delete_path_unlocked(&lock)
		.await
		.expect("release the path lock");
}
