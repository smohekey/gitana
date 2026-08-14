// Exercises the native cap-std backend; the wasm target has no `from_dir`.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::LocalFileStore;

fn temp_dir(tag: &str) -> std::path::PathBuf {
	let dir = std::env::temp_dir().join(format!(
		"gitana-file-hardening-{tag}-{}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	dir
}

/// Create `dir` and open it as a capability for a test (the store is capability-pure, so
/// tests mint the ambient authority themselves).
fn open_store(dir: &std::path::Path) -> LocalFileStore {
	std::fs::create_dir_all(dir).unwrap();
	LocalFileStore::from_dir(Dir::open_ambient_dir(dir, ambient_authority()).unwrap())
}

#[tokio::test]
async fn durability_barrier_flushes_state_from_before_store_open() {
	let dir = temp_dir("reopened-durability");
	std::fs::create_dir_all(dir.join("objects/aa")).unwrap();
	std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
	std::fs::write(dir.join("objects/aa/object"), b"object").unwrap();
	std::fs::write(dir.join("refs/heads/main"), b"commit\n").unwrap();

	let store = open_store(&dir);
	store.durability_barrier().await.unwrap();

	assert_eq!(
		store.read_path("objects/aa/object").await.unwrap(),
		b"object"
	);
	assert_eq!(
		store.read_path("refs/heads/main").await.unwrap(),
		b"commit\n"
	);

	let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rejects_path_traversal() {
	let dir = temp_dir("traversal");
	let store = open_store(&dir);

	for bad in ["../escape", "a/../../escape", "/etc/passwd", "a//b"] {
		assert!(
			matches!(store.read_path(bad).await, Err(FileStoreError::Backend(_))),
			"read of {bad:?} should be rejected"
		);
		assert!(
			matches!(
				store.write_path_if_absent(bad, b"x").await,
				Err(FileStoreError::Backend(_))
			),
			"write of {bad:?} should be rejected"
		);
	}

	let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escape() {
	let dir = temp_dir("symlink");
	let outside = temp_dir("symlink-outside");
	std::fs::create_dir_all(dir.join("repo1")).unwrap();
	std::fs::create_dir_all(&outside).unwrap();
	// A symlink inside the store that points outside it.
	std::os::unix::fs::symlink(&outside, dir.join("evil")).unwrap();

	let store = open_store(&dir);

	assert!(
		matches!(
			store.write_path_if_absent("evil/x", b"data").await,
			Err(FileStoreError::Backend(_))
		),
		"writing through a symlink that escapes the root must be rejected"
	);

	let _ = std::fs::remove_dir_all(&dir);
	let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cas_has_no_lost_updates() {
	let dir = temp_dir("cas");
	let store = Arc::new(open_store(&dir));
	let path = "refs/heads/counter";

	const TASKS: u64 = 8;
	const PER_TASK: u64 = 50;

	let mut handles = Vec::new();
	for _ in 0..TASKS {
		let store = Arc::clone(&store);
		handles.push(tokio::spawn(async move {
			for _ in 0..PER_TASK {
				loop {
					let (current, expected) = match store.read_path_versioned(path).await {
						Ok((bytes, version)) => {
							let n: u64 = std::str::from_utf8(&bytes).unwrap().parse().unwrap();
							(n, Some(version))
						}
						Err(FileStoreError::NotFound) => (0, None),
						Err(other) => panic!("unexpected read error: {other}"),
					};
					let next = (current + 1).to_string();
					match store
						.write_path_cas(path, next.as_bytes(), expected.as_ref())
						.await
					{
						Ok(_) => break,
						Err(FileStoreError::VersionMismatch) => continue,
						Err(other) => panic!("unexpected cas error: {other}"),
					}
				}
			}
		}));
	}
	for handle in handles {
		handle.await.unwrap();
	}

	let bytes = store.read_path(path).await.unwrap();
	let total: u64 = std::str::from_utf8(&bytes).unwrap().parse().unwrap();
	assert_eq!(total, TASKS * PER_TASK, "every increment must be durable");

	let _ = std::fs::remove_dir_all(&dir);
}
