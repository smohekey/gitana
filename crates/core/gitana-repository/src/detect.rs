//! Runtime hash-kind detection through the file store.

use gitana_file_store::FileStore;
use gitana_object::HashKind;

use crate::{Config, RepositoryError};

/// Read a repository's object-hash algorithm from its `config` through the file store,
/// without committing to a compile-time hash type — the capability-side counterpart of
/// the ambient-path detection at the CLI edge (`gta-core`'s `detect_algorithm`). git
/// treats an absent `extensions.objectformat` as sha1; [`Config::read`] refuses any
/// format other than sha1/sha256.
pub async fn detect_hash_kind(store: &impl FileStore) -> Result<HashKind, RepositoryError> {
	match Config::read(store).await?.object_format.as_str() {
		"sha256" => Ok(HashKind::Sha256),
		"sha1" => Ok(HashKind::Sha1),
		// Unreachable through `Config::read` (parse validates), kept for a `Config`
		// constructed by hand.
		other => Err(RepositoryError::UnsupportedFormat(format!(
			"objectformat = {other} (only sha1 and sha256 are supported)"
		))),
	}
}

#[cfg(test)]
mod tests {
	use gitana_file_store::FileStore;
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::HashKind;

	use super::detect_hash_kind;
	use crate::{Config, RepositoryError};

	async fn store_with_config(text: &str) -> MemoryFileStore {
		store_with_bytes(text.as_bytes()).await
	}

	async fn store_with_bytes(bytes: &[u8]) -> MemoryFileStore {
		let store = MemoryFileStore::new();
		store
			.write_path_if_absent("config", bytes)
			.await
			.expect("write config");
		store
	}

	#[tokio::test]
	async fn detects_sha256() {
		let store = store_with_config(&Config::sha256().render()).await;
		assert_eq!(detect_hash_kind(&store).await.unwrap(), HashKind::Sha256);
	}

	#[tokio::test]
	async fn detects_sha1() {
		let store = store_with_config(&Config::sha1().render()).await;
		assert_eq!(detect_hash_kind(&store).await.unwrap(), HashKind::Sha1);
	}

	#[tokio::test]
	async fn absent_objectformat_is_sha1() {
		let store = store_with_config("[core]\n\trepositoryformatversion = 0\n").await;
		assert_eq!(detect_hash_kind(&store).await.unwrap(), HashKind::Sha1);
	}

	#[tokio::test]
	async fn unrelated_non_utf8_values_do_not_hide_the_repository_format() {
		let store = store_with_bytes(
			b"[core]\n\trepositoryformatversion = 0\n[remote \"binary\"]\n\turl = \xff\n",
		)
		.await;
		assert_eq!(detect_hash_kind(&store).await.unwrap(), HashKind::Sha1);
	}

	#[tokio::test]
	async fn missing_config_is_unsupported() {
		let store = MemoryFileStore::new();
		assert!(matches!(
			detect_hash_kind(&store).await,
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}

	#[tokio::test]
	async fn unknown_format_is_unsupported() {
		let store = store_with_bytes(
			b"[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha999\n[remote \"binary\"]\n\turl = \xff\n",
		)
		.await;
		assert!(matches!(
			detect_hash_kind(&store).await,
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}
}
