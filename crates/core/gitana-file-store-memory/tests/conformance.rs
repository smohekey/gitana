use gitana_file_store_conformance::check_file_store;
use gitana_file_store_memory::MemoryFileStore;

#[tokio::test]
async fn memory_file_store_satisfies_contract() {
	check_file_store(&MemoryFileStore::new()).await;
}
