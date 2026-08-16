use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use gitana_file_store::{
	ByteReader, DeleteOutcome, DurabilityTarget, FileStore, PathLock, Result, Version, WriteOutcome,
};
use gitana_file_store_memory::MemoryFileStore;

struct WriteGate {
	started: AtomicBool,
	released: AtomicBool,
	waker: Mutex<Option<Waker>>,
}

/// Test file store that pauses unconditional replacement after a ref transaction owns its lock.
pub(crate) struct GatedFileStore {
	inner: Arc<MemoryFileStore>,
	gate: Arc<WriteGate>,
}

impl GatedFileStore {
	pub(crate) fn new() -> Self {
		Self {
			inner: Arc::new(MemoryFileStore::new()),
			gate: Arc::new(WriteGate {
				started: AtomicBool::new(false),
				released: AtomicBool::new(false),
				waker: Mutex::new(None),
			}),
		}
	}

	pub(crate) async fn wait_until_blocked(&self) {
		while !self.gate.started.load(Ordering::Acquire) {
			tokio::task::yield_now().await;
		}
	}

	pub(crate) fn release(&self) {
		self.gate.released.store(true, Ordering::Release);
		if let Some(waker) = self.gate.waker.lock().expect("gate lock poisoned").take() {
			waker.wake();
		}
	}

	async fn wait_for_release(&self) {
		self.gate.started.store(true, Ordering::Release);
		poll_fn(|cx| {
			if self.gate.released.load(Ordering::Acquire) {
				return Poll::Ready(());
			}
			*self.gate.waker.lock().expect("gate lock poisoned") = Some(cx.waker().clone());
			if self.gate.released.load(Ordering::Acquire) {
				Poll::Ready(())
			} else {
				Poll::Pending
			}
		})
		.await;
	}
}

impl FileStore for GatedFileStore {
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			inner: Arc::clone(&self.inner),
			gate: Arc::clone(&self.gate),
		}
	}

	fn durability_barrier(
		&self,
		targets: &[DurabilityTarget],
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.durability_barrier(targets)
	}

	fn read_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> {
		self.inner.read_path(path)
	}

	fn read_path_versioned(&self, path: &str) -> impl Future<Output = Result<(Vec<u8>, Version)>> {
		self.inner.read_path_versioned(path)
	}

	fn write_path_if_absent(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<WriteOutcome>> {
		self.inner.write_path_if_absent(path, bytes)
	}

	fn try_lock_path(&self, path: &str) -> impl Future<Output = Result<Option<PathLock>>> {
		self.inner.try_lock_path(path)
	}

	fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> impl Future<Output = Result<Version>> {
		self.inner.write_path_cas(path, bytes, expected)
	}

	async fn write_path_replace(&self, path: &str, bytes: &[u8]) -> Result<()> {
		self.wait_for_release().await;
		self.inner.write_path_replace(path, bytes).await
	}

	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> {
		self.inner.delete_path(path, expected)
	}

	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> {
		self.inner.delete_path_unlocked(path)
	}

	fn remove_dir(&self, path: &str) -> impl Future<Output = Result<()>> {
		self.inner.remove_dir(path)
	}

	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> {
		self.inner.exists(path)
	}

	fn is_dir(&self, path: &str) -> impl Future<Output = Result<bool>> {
		self.inner.is_dir(path)
	}

	fn size(&self, path: &str) -> impl Future<Output = Result<u64>> {
		self.inner.size(path)
	}

	fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> {
		self.inner.list_prefix(prefix)
	}

	fn read_path_range(
		&self,
		path: &str,
		offset: u64,
		length: u64,
	) -> impl Future<Output = Result<Vec<u8>>> {
		self.inner.read_path_range(path, offset, length)
	}

	fn read_path_stream(&self, path: &str) -> impl Future<Output = Result<ByteReader>> {
		self.inner.read_path_stream(path)
	}

	fn write_path_stream_if_absent(
		&self,
		path: &str,
		reader: ByteReader,
		max_len: u64,
	) -> impl Future<Output = Result<WriteOutcome>> {
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
	) -> impl Future<Output = Result<()>> {
		self.inner.replace_and_release_lock(path, bytes, lock_path)
	}
}
