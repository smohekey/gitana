use std::{
	future::Future,
	sync::{Arc, Mutex},
};

use gitana_file_store::{
	ByteReader, DeleteOutcome, DurabilityTarget, FileStore, PathLock, Result, Version, WriteOutcome,
};
use gitana_file_store_memory::MemoryFileStore;

/// A memory file store that records durability barriers and deletions in the order they happen.
pub(crate) struct RecordingFileStore {
	inner: MemoryFileStore,
	events: Arc<Mutex<Vec<RecordedEvent>>>,
}

/// One recorded file-store operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordedEvent {
	Barrier(Vec<DurabilityTarget>),
	Delete(String),
}

impl RecordingFileStore {
	pub(crate) fn new() -> Self {
		Self {
			inner: MemoryFileStore::new(),
			events: Arc::new(Mutex::new(Vec::new())),
		}
	}

	pub(crate) fn events(&self) -> Vec<RecordedEvent> {
		self.events.lock().expect("event log lock poisoned").clone()
	}

	fn record(&self, event: RecordedEvent) {
		self
			.events
			.lock()
			.expect("event log lock poisoned")
			.push(event);
	}
}

impl FileStore for RecordingFileStore {
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			inner: self.inner.shared_handle(),
			events: Arc::clone(&self.events),
		}
	}

	fn durability_barrier(
		&self,
		targets: &[DurabilityTarget],
	) -> impl Future<Output = Result<()>> + Send {
		self.record(RecordedEvent::Barrier(targets.to_vec()));
		self.inner.durability_barrier(targets)
	}

	fn read_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_path(path)
	}

	fn read_path_versioned(
		&self,
		path: &str,
	) -> impl Future<Output = Result<(Vec<u8>, Version)>> + Send {
		self.inner.read_path_versioned(path)
	}

	fn write_path_if_absent(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<WriteOutcome>> + Send {
		self.inner.write_path_if_absent(path, bytes)
	}

	fn try_lock_path(&self, path: &str) -> impl Future<Output = Result<Option<PathLock>>> + Send {
		self.inner.try_lock_path(path)
	}

	fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> impl Future<Output = Result<Version>> + Send {
		self.inner.write_path_cas(path, bytes, expected)
	}

	fn write_path_replace(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.write_path_replace(path, bytes)
	}

	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> + Send {
		self.record(RecordedEvent::Delete(path.to_owned()));
		self.inner.delete_path(path, expected)
	}

	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> + Send {
		self.record(RecordedEvent::Delete(path.to_owned()));
		self.inner.delete_path_unlocked(path)
	}

	fn remove_dir(&self, path: &str) -> impl Future<Output = Result<()>> + Send {
		self.inner.remove_dir(path)
	}

	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> + Send {
		self.inner.exists(path)
	}

	fn is_dir(&self, path: &str) -> impl Future<Output = Result<bool>> + Send {
		self.inner.is_dir(path)
	}

	fn size(&self, path: &str) -> impl Future<Output = Result<u64>> + Send {
		self.inner.size(path)
	}

	fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send {
		self.inner.list_prefix(prefix)
	}

	fn list_prefix_bounded(
		&self,
		prefix: &str,
		max_entries: usize,
		max_bytes: u64,
	) -> impl Future<Output = Result<Vec<String>>> + Send {
		self
			.inner
			.list_prefix_bounded(prefix, max_entries, max_bytes)
	}

	fn read_path_range(
		&self,
		path: &str,
		offset: u64,
		length: u64,
	) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_path_range(path, offset, length)
	}

	fn read_path_stream(&self, path: &str) -> impl Future<Output = Result<ByteReader>> + Send {
		self.inner.read_path_stream(path)
	}

	fn write_path_stream_if_absent(
		&self,
		path: &str,
		reader: ByteReader,
		max_len: u64,
	) -> impl Future<Output = Result<WriteOutcome>> + Send {
		self
			.inner
			.write_path_stream_if_absent(path, reader, max_len)
	}

	fn remove_lock_file_sync(&self, path: &str) {
		self.inner.remove_lock_file_sync(path);
	}

	fn replace_and_release_lock(
		&self,
		path: &str,
		bytes: &[u8],
		lock_path: &str,
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.replace_and_release_lock(path, bytes, lock_path)
	}
}
