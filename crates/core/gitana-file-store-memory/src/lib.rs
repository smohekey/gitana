//! In-memory [`FileStore`] backend for tests and local CI.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use gitana_file_store::{
	ByteReader, DeleteOutcome, DurabilityTarget, FileStore, FileStoreError, PathLock, Result,
	Version, WriteOutcome,
};
use tokio::io::AsyncReadExt;

type Key = String;
type StoredValue = (Vec<u8>, Version);
type DirectoryChildren = HashMap<String, BTreeMap<String, usize>>;
type State = Arc<RwLock<MemoryState>>;

#[derive(Default)]
struct MemoryState {
	files: HashMap<Key, StoredValue>,
	directory_children: DirectoryChildren,
}

/// A `FileStore` that keeps every value in process memory.
#[derive(Default)]
pub struct MemoryFileStore {
	state: State,
	next_version: Arc<AtomicU64>,
}

fn visit_directory_entries(path: &str, mut visit: impl FnMut(&str, &str)) {
	let mut child_start = 0;
	loop {
		let child_end = path[child_start..]
			.find('/')
			.map_or(path.len(), |relative| child_start + relative);
		visit(&path[..child_start], &path[child_start..child_end]);
		if child_end == path.len() {
			break;
		}
		child_start = child_end + 1;
	}
}

fn index_path(directory_children: &mut DirectoryChildren, path: &str) {
	visit_directory_entries(path, |directory, child| {
		let count = directory_children
			.entry(directory.to_owned())
			.or_default()
			.entry(child.to_owned())
			.or_default();
		*count = count.saturating_add(1);
	});
}

fn unindex_path(directory_children: &mut DirectoryChildren, path: &str) {
	visit_directory_entries(path, |directory, child| {
		let remove_directory = if let Some(children) = directory_children.get_mut(directory) {
			if let Some(count) = children.get_mut(child) {
				if *count == 1 {
					children.remove(child);
				} else {
					*count -= 1;
				}
			}
			children.is_empty()
		} else {
			false
		};
		if remove_directory {
			directory_children.remove(directory);
		}
	});
}

fn insert_value(state: &mut MemoryState, key: Key, value: StoredValue) -> Option<StoredValue> {
	let is_new = !state.files.contains_key(&key);
	let previous = state.files.insert(key.clone(), value);
	if is_new {
		index_path(&mut state.directory_children, &key);
	}
	previous
}

fn remove_value(state: &mut MemoryState, key: &str) -> Option<StoredValue> {
	let removed = state.files.remove(key);
	if removed.is_some() {
		unindex_path(&mut state.directory_children, key);
	}
	removed
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
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			state: Arc::clone(&self.state),
			next_version: Arc::clone(&self.next_version),
		}
	}

	async fn durability_barrier(&self, _targets: &[DurabilityTarget]) -> Result<()> {
		// Mutations are complete as soon as the in-memory map lock is released; there is no
		// persistence layer for a caller-controlled barrier to flush.
		Ok(())
	}

	async fn read_path(&self, path: &str) -> Result<Vec<u8>> {
		self
			.state
			.read()
			.expect("file store lock poisoned")
			.files
			.get(path)
			.map(|(bytes, _)| bytes.clone())
			.ok_or(FileStoreError::NotFound)
	}

	async fn read_path_versioned(&self, path: &str) -> Result<(Vec<u8>, Version)> {
		self
			.state
			.read()
			.expect("file store lock poisoned")
			.files
			.get(path)
			.map(|(bytes, version)| (bytes.clone(), version.clone()))
			.ok_or(FileStoreError::NotFound)
	}

	async fn write_path_if_absent(&self, path: &str, bytes: &[u8]) -> Result<WriteOutcome> {
		let version = self.mint_version();
		let mut state = self.state.write().expect("file store lock poisoned");
		if state.files.contains_key(path) {
			Ok(WriteOutcome::AlreadyExists)
		} else {
			insert_value(&mut state, path.to_owned(), (bytes.to_vec(), version));
			Ok(WriteOutcome::Written)
		}
	}

	async fn try_lock_path(&self, path: &str) -> Result<Option<PathLock>> {
		let version = self.mint_version();
		let key = path.to_owned();
		let mut state = self.state.write().expect("file store lock poisoned");
		if state.files.contains_key(&key) {
			return Ok(None);
		}
		insert_value(&mut state, key.clone(), (Vec::new(), version));
		drop(state);

		let state = Arc::clone(&self.state);
		Ok(Some(PathLock::new(move || {
			let mut state = state.write().expect("file store lock poisoned");
			remove_value(&mut state, &key);
		})))
	}

	async fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> Result<Version> {
		let version = self.mint_version();
		let mut state = self.state.write().expect("file store lock poisoned");
		let key = path.to_owned();
		let current = state.files.get(&key).map(|(_, version)| version);
		if expected != current {
			return Err(FileStoreError::VersionMismatch);
		}
		insert_value(&mut state, key, (bytes.to_vec(), version.clone()));
		Ok(version)
	}

	async fn write_path_replace(&self, path: &str, bytes: &[u8]) -> Result<()> {
		// The in-memory map is already an atomic overwrite under the write lock, so this is a
		// plain unconditional insert — no version check, no lock file.
		let version = self.mint_version();
		let mut state = self.state.write().expect("file store lock poisoned");
		insert_value(&mut state, path.to_owned(), (bytes.to_vec(), version));
		Ok(())
	}

	async fn delete_path(&self, path: &str, expected: Option<&Version>) -> Result<DeleteOutcome> {
		let mut state = self.state.write().expect("file store lock poisoned");
		let key = path.to_owned();
		match state.files.get(&key) {
			None => Ok(DeleteOutcome::NotFound),
			Some((_, current)) => {
				if let Some(expected) = expected
					&& expected != current
				{
					return Err(FileStoreError::VersionMismatch);
				}
				remove_value(&mut state, &key);
				Ok(DeleteOutcome::Deleted)
			}
		}
	}

	async fn delete_path_unlocked(&self, path: &str) -> Result<DeleteOutcome> {
		// The map removal is already atomic under the write lock; there are no `<path>.lock` files in
		// the memory backend, so this is just an unconditional remove.
		let mut state = self.state.write().expect("file store lock poisoned");
		match remove_value(&mut state, path) {
			Some(_) => Ok(DeleteOutcome::Deleted),
			None => Ok(DeleteOutcome::NotFound),
		}
	}

	async fn exists(&self, path: &str) -> Result<bool> {
		Ok(
			self
				.state
				.read()
				.expect("file store lock poisoned")
				.files
				.contains_key(path),
		)
	}

	async fn is_dir(&self, path: &str) -> Result<bool> {
		// The map has no physical directories, but descendant keys imply the same logical directory
		// namespace as a filesystem-backed store. Report that shape so callers cannot create both a
		// value and one of its ancestor/descendant paths.
		if path.is_empty() {
			return Ok(false);
		}
		let directory = format!("{path}/");
		Ok(
			self
				.state
				.read()
				.expect("file store lock poisoned")
				.directory_children
				.contains_key(&directory),
		)
	}

	async fn remove_dir(&self, _path: &str) -> Result<()> {
		// No directories exist to remove; report absence so a best-effort pruner stops.
		Err(FileStoreError::NotFound)
	}

	async fn size(&self, path: &str) -> Result<u64> {
		self
			.state
			.read()
			.expect("file store lock poisoned")
			.files
			.get(path)
			.map(|(bytes, _)| bytes.len() as u64)
			.ok_or(FileStoreError::NotFound)
	}

	async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
		let (dir, frag) = gitana_file_store::split_prefix(prefix);
		// Return the immediate children of `dir`: a stored file becomes its full path; a
		// nested key contributes its first path segment as a (synthetic) subdirectory
		// entry, deduped — mirroring a real directory listing (as the file backend's
		// `read_dir` yields), so callers like `RefStore::list` can walk the tree.
		let state = self.state.read().expect("file store lock poisoned");
		let Some(children) = state.directory_children.get(dir) else {
			return Ok(Vec::new());
		};
		Ok(
			children
				.keys()
				.filter(|name| name.starts_with(frag))
				.map(|name| format!("{dir}{name}"))
				.collect(),
		)
	}

	async fn list_prefix_bounded(
		&self,
		prefix: &str,
		max_entries: usize,
		max_bytes: u64,
	) -> Result<Vec<String>> {
		let (dir, frag) = gitana_file_store::split_prefix(prefix);
		let state = self.state.read().expect("file store lock poisoned");
		let Some(children) = state.directory_children.get(dir) else {
			return Ok(Vec::new());
		};
		let mut out = Vec::new();
		let mut bytes = 0u64;
		for (count, name) in children.keys().enumerate() {
			let next_bytes = bytes.saturating_add(name.len() as u64);
			if count >= max_entries || next_bytes > max_bytes {
				return Err(FileStoreError::ListingTooLarge {
					max_entries,
					max_bytes,
				});
			}
			bytes = next_bytes;
			if name.starts_with(frag) {
				out.push(format!("{dir}{name}"));
			}
		}
		Ok(out)
	}

	async fn read_path_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
		let state = self.state.read().expect("file store lock poisoned");
		let bytes = state
			.files
			.get(path)
			.map(|(bytes, _)| bytes)
			.ok_or(FileStoreError::NotFound)?;
		let start = usize::try_from(offset)
			.unwrap_or(usize::MAX)
			.min(bytes.len());
		let length = usize::try_from(length).unwrap_or(usize::MAX);
		let end = start.saturating_add(length).min(bytes.len());
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

	fn remove_lock_file_sync(&self, path: &str) {
		// Synchronous unconditional removal — the map write lock is already sync, so a `Drop`-time
		// release needs no async path. Absent key → nothing to do.
		let mut state = self.state.write().expect("file store lock poisoned");
		remove_value(&mut state, path);
	}

	async fn replace_and_release_lock(
		&self,
		path: &str,
		bytes: &[u8],
		lock_path: &str,
	) -> Result<()> {
		// One write-lock critical section makes the replace and the lock removal atomic and infallible;
		// there is no blocking task to outlive cancellation, so nothing here can be interrupted mid-way.
		let version = self.mint_version();
		let mut state = self.state.write().expect("file store lock poisoned");
		insert_value(&mut state, path.to_owned(), (bytes.to_vec(), version));
		remove_value(&mut state, lock_path);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use gitana_file_store::{FileStore, FileStoreError};

	use super::MemoryFileStore;

	#[tokio::test]
	async fn bounded_listing_uses_the_immediate_directory_index() {
		let store = MemoryFileStore::new();
		for index in 0..32 {
			store
				.write_path_if_absent(&format!("objects/pack/nested/object-{index}"), b"object")
				.await
				.expect("write descendant");
			store
				.write_path_if_absent(&format!("refs/heads/branch-{index}"), b"ref")
				.await
				.expect("write unrelated path");
		}
		store
			.write_path_replace("objects/pack/nested/object-0", b"replacement")
			.await
			.expect("replace descendant");

		assert_eq!(
			store
				.list_prefix_bounded("objects/pack/", 1, "nested".len() as u64)
				.await
				.expect("bounded listing"),
			vec!["objects/pack/nested".to_owned()]
		);
		assert!(matches!(
			store.list_prefix_bounded("objects/pack/", 0, 0).await,
			Err(FileStoreError::ListingTooLarge {
				max_entries: 0,
				max_bytes: 0,
			})
		));
		assert!(
			store
				.list_prefix_bounded("missing/", 0, 0)
				.await
				.expect("empty bounded listing")
				.is_empty()
		);

		for index in 0..32 {
			store
				.delete_path(&format!("objects/pack/nested/object-{index}"), None)
				.await
				.expect("delete descendant");
		}
		assert!(
			store
				.list_prefix("objects/pack/")
				.await
				.expect("list after deletion")
				.is_empty()
		);
		assert!(
			!store
				.is_dir("objects/pack/nested")
				.await
				.expect("directory state")
		);
	}
}
