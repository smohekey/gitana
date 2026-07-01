// Exercises the native cap-std backend; the wasm target has no `from_dir`.
#![cfg(not(target_arch = "wasm32"))]

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store_conformance::check_file_store;
use gitana_file_store_local::LocalFileStore;

#[tokio::test]
async fn file_file_store_satisfies_contract() {
	let dir = std::env::temp_dir().join(format!("gitana-file-store-test-{}", std::process::id()));
	// Start from a clean directory so immutable-write checks see fresh paths.
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();

	let store = LocalFileStore::from_dir(Dir::open_ambient_dir(&dir, ambient_authority()).unwrap());
	check_file_store(&store).await;

	let _ = std::fs::remove_dir_all(&dir);
}
