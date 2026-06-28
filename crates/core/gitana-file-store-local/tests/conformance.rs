use gitana_file_store_conformance::check_file_store;
use gitana_file_store_local::LocalFileStore;

#[tokio::test]
async fn file_file_store_satisfies_contract() {
	let dir = std::env::temp_dir().join(format!("gitana-file-store-test-{}", std::process::id()));
	// Start from a clean directory so immutable-write checks see fresh paths.
	let _ = std::fs::remove_dir_all(&dir);

	check_file_store(&LocalFileStore::new(&dir)).await;

	let _ = std::fs::remove_dir_all(&dir);
}
