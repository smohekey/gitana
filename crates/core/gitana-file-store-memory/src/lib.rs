//! In-memory [`FileStore`] backend for tests and local CI.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use gitana_file_store::{
	ByteReader, DeleteOutcome, FileStore, FileStoreError, Result, Version, WriteOutcome,
};
use tokio::io::AsyncReadExt;

type Key = String;

/// A `FileStore` that keeps every value in process memory.
#[derive(Default)]
pub struct MemoryFileStore {
	files: RwLock<HashMap<Key, (Vec<u8>, Version)>>,
	next_version: AtomicU64,
}

impl MemoryFileStore {
	/// An empty in-memory store.
	pub fn new() -> Self {
		Self::default()
	}

	fn mint_version(&self) -> Version {
		Version(
			self
				.next_version
				.fetch_add(1, Ordering::Relaxed)
				.to_string()
				.into(),
		)
	}
}

impl FileStore for MemoryFileStore {
	async fn read_path(&self, path: &str) -> Result<Vec<u8>> {
		self
			.files
			.read()
			.expect("file store lock poisoned")
			.get(&path.to_owned())
			.map(|(bytes, _)| bytes.clone())
			.ok_or(FileStoreError::NotFound)
	}

	async fn read_path_versioned(&self, path: &str) -> Result<(Vec<u8>, Version)> {
		self
			.files
			.read()
			.expect("file store lock poisoned")
			.get(&path.to_owned())
			.map(|(bytes, version)| (bytes.clone(), version.clone()))
			.ok_or(FileStoreError::NotFound)
	}

	async fn write_path_if_absent(&self, path: &str, bytes: &[u8]) -> Result<WriteOutcome> {
		let version = self.mint_version();
		let mut files = self.files.write().expect("file store lock poisoned");
		match files.entry(path.to_owned()) {
			std::collections::hash_map::Entry::Occupied(_) => Ok(WriteOutcome::AlreadyExists),
			std::collections::hash_map::Entry::Vacant(slot) => {
				slot.insert((bytes.to_vec(), version));
				Ok(WriteOutcome::Written)
			}
		}
	}

	async fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> Result<Version> {
		let version = self.mint_version();
		let mut files = self.files.write().expect("file store lock poisoned");
		let key = path.to_owned();
		let current = files.get(&key).map(|(_, version)| version);
		if expected != current {
			return Err(FileStoreError::VersionMismatch);
		}
		files.insert(key, (bytes.to_vec(), version.clone()));
		Ok(version)
	}

	async fn write_path_replace(&self, path: &str, bytes: &[u8]) -> Result<()> {
		// The in-memory map is already an atomic overwrite under the write lock, so this is a
		// plain unconditional insert — no version check, no lock file.
		let version = self.mint_version();
		self
			.files
			.write()
			.expect("file store lock poisoned")
			.insert(path.to_owned(), (bytes.to_vec(), version));
		Ok(())
	}

	async fn delete_path(&self, path: &str, expected: Option<&Version>) -> Result<DeleteOutcome> {
		let mut files = self.files.write().expect("file store lock poisoned");
		let key = path.to_owned();
		match files.get(&key) {
			None => Ok(DeleteOutcome::NotFound),
			Some((_, current)) => {
				if let Some(expected) = expected
					&& expected != current
				{
					return Err(FileStoreError::VersionMismatch);
				}
				files.remove(&key);
				Ok(DeleteOutcome::Deleted)
			}
		}
	}

	async fn delete_path_unlocked(&self, path: &str) -> Result<DeleteOutcome> {
		// The map removal is already atomic under the write lock; there are no `<path>.lock` files in
		// the memory backend, so this is just an unconditional remove.
		let mut files = self.files.write().expect("file store lock poisoned");
		match files.remove(&path.to_owned()) {
			Some(_) => Ok(DeleteOutcome::Deleted),
			None => Ok(DeleteOutcome::NotFound),
		}
	}

	async fn exists(&self, path: &str) -> Result<bool> {
		Ok(
			self
				.files
				.read()
				.expect("file store lock poisoned")
				.contains_key(&path.to_owned()),
		)
	}

	async fn is_dir(&self, _path: &str) -> Result<bool> {
		// The in-memory store is a flat key→value map with no directories.
		Ok(false)
	}

	async fn remove_dir(&self, _path: &str) -> Result<()> {
		// No directories exist to remove; report absence so a best-effort pruner stops.
		Err(FileStoreError::NotFound)
	}

	async fn size(&self, path: &str) -> Result<u64> {
		self
			.files
			.read()
			.expect("file store lock poisoned")
			.get(&path.to_owned())
			.map(|(bytes, _)| bytes.len() as u64)
			.ok_or(FileStoreError::NotFound)
	}

	async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
		let (dir, frag) = gitana_file_store::split_prefix(prefix);
		// Return the immediate children of `dir`: a stored file becomes its full path; a
		// nested key contributes its first path segment as a (synthetic) subdirectory
		// entry, deduped — mirroring a real directory listing (as the file backend's
		// `read_dir` yields), so callers like `RefStore::list` can walk the tree.
		let mut children = std::collections::BTreeSet::new();
		for path in self.files.read().expect("file store lock poisoned").keys() {
			let Some(rest) = path.strip_prefix(dir) else {
				continue;
			};
			let first = rest.split('/').next().unwrap_or(rest);
			if first.starts_with(frag) {
				children.insert(format!("{dir}{first}"));
			}
		}
		Ok(children.into_iter().collect())
	}

	async fn read_path_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
		let bytes = self.read_path(path).await?;
		let start = (offset as usize).min(bytes.len());
		let end = start.saturating_add(length as usize).min(bytes.len());
		Ok(bytes[start..end].to_vec())
	}

	async fn read_path_stream(&self, path: &str) -> Result<ByteReader> {
		let bytes = self.read_path(path).await?;
		Ok(Box::new(std::io::Cursor::new(bytes)))
	}

	async fn write_path_stream_if_absent(
		&self,
		path: &str,
		mut reader: ByteReader,
		max_len: u64,
	) -> Result<WriteOutcome> {
		let mut buf = Vec::new();
		let mut chunk = [0u8; 64 * 1024];
		loop {
			let n = reader
				.read(&mut chunk)
				.await
				.map_err(|error| FileStoreError::Backend(error.to_string()))?;
			if n == 0 {
				break;
			}
			if buf.len() as u64 + n as u64 > max_len {
				return Err(FileStoreError::TooLarge { limit: max_len });
			}
			buf.extend_from_slice(&chunk[..n]);
		}
		self.write_path_if_absent(path, &buf).await
	}
}
