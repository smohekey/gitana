use std::{
	future::Future,
	sync::{Arc, Mutex},
};

use gitana_file_store::{
	ByteReader, DeleteOutcome, DurabilityTarget, FileStore, PathLock, Result, Version, WriteOutcome,
};
use gitana_file_store_memory::MemoryFileStore;

pub(crate) struct DisappearingLooseFileStore {
	inner: MemoryFileStore,
	disappear_on_range_read: Arc<Mutex<Option<String>>>,
	pack_transition: Arc<Mutex<Option<PackTransition>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PackTransitionPoint {
	Size,
	RangeRead,
}

struct PackTransition {
	old_pack: String,
	old_index: String,
	replacement: Vec<(String, Vec<u8>)>,
	point: PackTransitionPoint,
}

impl DisappearingLooseFileStore {
	pub(crate) fn new() -> Self {
		Self {
			inner: MemoryFileStore::new(),
			disappear_on_range_read: Arc::new(Mutex::new(None)),
			pack_transition: Arc::new(Mutex::new(None)),
		}
	}

	pub(crate) fn disappear_on_next_range_read(&self, path: &str) {
		*self
			.disappear_on_range_read
			.lock()
			.expect("disappearing path lock poisoned") = Some(path.to_owned());
	}

	pub(crate) fn replace_pack_on_next_size(
		&self,
		old_pack: &str,
		old_index: &str,
		replacement: Vec<(String, Vec<u8>)>,
	) {
		self.arm_pack_transition(old_pack, old_index, replacement, PackTransitionPoint::Size);
	}

	pub(crate) fn replace_pack_on_next_range_read(
		&self,
		old_pack: &str,
		old_index: &str,
		replacement: Vec<(String, Vec<u8>)>,
	) {
		self.arm_pack_transition(
			old_pack,
			old_index,
			replacement,
			PackTransitionPoint::RangeRead,
		);
	}

	fn arm_pack_transition(
		&self,
		old_pack: &str,
		old_index: &str,
		replacement: Vec<(String, Vec<u8>)>,
		point: PackTransitionPoint,
	) {
		*self
			.pack_transition
			.lock()
			.expect("pack transition lock poisoned") = Some(PackTransition {
			old_pack: old_pack.to_owned(),
			old_index: old_index.to_owned(),
			replacement,
			point,
		});
	}

	async fn transition_pack(&self, path: &str, point: PackTransitionPoint) -> Result<()> {
		let transition = {
			let mut armed = self
				.pack_transition
				.lock()
				.expect("pack transition lock poisoned");
			if armed
				.as_ref()
				.is_some_and(|transition| transition.old_pack == path && transition.point == point)
			{
				armed.take()
			} else {
				None
			}
		};
		let Some(transition) = transition else {
			return Ok(());
		};

		for (path, bytes) in transition.replacement {
			self.inner.write_path_replace(&path, &bytes).await?;
		}
		self.inner.delete_path(&transition.old_pack, None).await?;
		self.inner.delete_path(&transition.old_index, None).await?;
		Ok(())
	}
}

impl FileStore for DisappearingLooseFileStore {
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			inner: self.inner.shared_handle(),
			disappear_on_range_read: Arc::clone(&self.disappear_on_range_read),
			pack_transition: Arc::clone(&self.pack_transition),
		}
	}

	fn durability_barrier(
		&self,
		targets: &[DurabilityTarget],
	) -> impl Future<Output = Result<()>> + Send {
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
		self.inner.delete_path(path, expected)
	}

	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> + Send {
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

	async fn size(&self, path: &str) -> Result<u64> {
		self
			.transition_pack(path, PackTransitionPoint::Size)
			.await?;
		self.inner.size(path).await
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

	async fn read_path_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
		self
			.transition_pack(path, PackTransitionPoint::RangeRead)
			.await?;
		let should_remove = {
			let mut armed = self
				.disappear_on_range_read
				.lock()
				.expect("disappearing path lock poisoned");
			if armed.as_deref() == Some(path) {
				armed.take();
				true
			} else {
				false
			}
		};
		if should_remove {
			self.inner.delete_path(path, None).await?;
		}
		self.inner.read_path_range(path, offset, length).await
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
