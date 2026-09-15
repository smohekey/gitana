use std::io::{Read, Write};
use std::sync::Arc;

use gitana_file_store::Result;

use crate::{Backend, FileKind};

pub(crate) struct WorkerKeepaliveBackend {
	inner: Arc<dyn Backend>,
	_keepalive: Arc<dyn Send + Sync>,
}

impl WorkerKeepaliveBackend {
	pub(crate) fn new(inner: Arc<dyn Backend>, keepalive: Arc<dyn Send + Sync>) -> Self {
		Self {
			inner,
			_keepalive: keepalive,
		}
	}
}

impl Backend for WorkerKeepaliveBackend {
	fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
		self.inner.read(path)
	}

	fn read_range(&self, path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
		self.inner.read_range(path, offset, length)
	}

	fn create_dir_all(&self, path: &str) -> std::io::Result<()> {
		self.inner.create_dir_all(path)
	}

	fn create_new(&self, path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>> {
		self.inner.create_new(path)
	}

	fn open_read(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>> {
		self.inner.open_read(path)
	}

	fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
		self.inner.rename(from, to)
	}

	fn remove_file(&self, path: &str) -> std::io::Result<()> {
		self.inner.remove_file(path)
	}

	fn remove_dir(&self, path: &str) -> std::io::Result<()> {
		self.inner.remove_dir(path)
	}

	fn exists(&self, path: &str) -> std::io::Result<bool> {
		self.inner.exists(path)
	}

	fn is_dir(&self, path: &str) -> std::io::Result<bool> {
		self.inner.is_dir(path)
	}

	fn size(&self, path: &str) -> std::io::Result<u64> {
		self.inner.size(path)
	}

	fn list_names(&self, path: &str) -> std::io::Result<Vec<String>> {
		self.inner.list_names(path)
	}

	fn list_names_bounded(
		&self,
		path: &str,
		max_entries: usize,
		max_bytes: u64,
	) -> Result<Vec<String>> {
		self.inner.list_names_bounded(path, max_entries, max_bytes)
	}

	fn kind(&self, path: &str) -> std::io::Result<FileKind> {
		self.inner.kind(path)
	}

	fn sync_file(&self, path: &str) -> std::io::Result<()> {
		self.inner.sync_file(path)
	}

	fn sync_dir(&self, path: &str) -> std::io::Result<()> {
		self.inner.sync_dir(path)
	}
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use std::io::{Cursor, Error, ErrorKind};
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Arc, Condvar, Mutex};

	use gitana_file_store::FileStore;

	use super::*;
	use crate::LocalFileStore;

	struct PausingPublicationBackend {
		entered: Arc<AtomicBool>,
		release: Arc<(Mutex<bool>, Condvar)>,
	}

	impl PausingPublicationBackend {
		fn unsupported<T>() -> std::io::Result<T> {
			Err(Error::new(ErrorKind::Unsupported, "unused test operation"))
		}
	}

	impl Backend for PausingPublicationBackend {
		fn read(&self, _path: &str) -> std::io::Result<Vec<u8>> {
			Self::unsupported()
		}

		fn read_range(&self, _path: &str, _offset: u64, _length: u64) -> std::io::Result<Vec<u8>> {
			Self::unsupported()
		}

		fn create_dir_all(&self, _path: &str) -> std::io::Result<()> {
			Ok(())
		}

		fn create_new(&self, _path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>> {
			Ok(Some(Box::new(Cursor::new(Vec::new()))))
		}

		fn open_read(&self, _path: &str) -> std::io::Result<Box<dyn Read + Send>> {
			Self::unsupported()
		}

		fn rename(&self, _from: &str, to: &str) -> std::io::Result<()> {
			if to == "config" {
				self.entered.store(true, Ordering::SeqCst);
				let (lock, ready) = &*self.release;
				let mut released = lock.lock().unwrap();
				while !*released {
					released = ready.wait(released).unwrap();
				}
			}
			Ok(())
		}

		fn remove_file(&self, _path: &str) -> std::io::Result<()> {
			Ok(())
		}

		fn remove_dir(&self, _path: &str) -> std::io::Result<()> {
			Self::unsupported()
		}

		fn exists(&self, _path: &str) -> std::io::Result<bool> {
			Self::unsupported()
		}

		fn is_dir(&self, _path: &str) -> std::io::Result<bool> {
			Self::unsupported()
		}

		fn size(&self, _path: &str) -> std::io::Result<u64> {
			Self::unsupported()
		}

		fn list_names(&self, _path: &str) -> std::io::Result<Vec<String>> {
			Self::unsupported()
		}

		fn kind(&self, path: &str) -> std::io::Result<FileKind> {
			if path == "config" {
				Err(Error::new(ErrorKind::NotFound, path.to_owned()))
			} else {
				Ok(FileKind::File)
			}
		}

		fn sync_file(&self, _path: &str) -> std::io::Result<()> {
			Self::unsupported()
		}

		fn sync_dir(&self, _path: &str) -> std::io::Result<()> {
			Self::unsupported()
		}
	}

	#[tokio::test]
	async fn cancelled_cas_retains_its_keepalive_until_the_worker_finishes() {
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new((Mutex::new(false), Condvar::new()));
		let owner = Arc::new(());
		let weak_owner = Arc::downgrade(&owner);
		let backend = Arc::new(PausingPublicationBackend {
			entered: Arc::clone(&entered),
			release: Arc::clone(&release),
		});
		let store = Arc::new(LocalFileStore::with_backend(backend).with_worker_keepalive(owner));
		let task_store = Arc::clone(&store);
		let task = tokio::spawn(async move {
			task_store
				.write_path_cas("config", b"new config", None)
				.await
		});
		for _ in 0..10_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(entered.load(Ordering::SeqCst), "CAS worker did not start");

		task.abort();
		assert!(task.await.unwrap_err().is_cancelled());
		drop(store);
		assert!(
			weak_owner.upgrade().is_some(),
			"the detached publication worker must retain serialization"
		);

		let (lock, ready) = &*release;
		*lock.lock().unwrap() = true;
		ready.notify_all();
		for _ in 0..10_000 {
			if weak_owner.upgrade().is_none() {
				return;
			}
			tokio::task::yield_now().await;
		}
		panic!("publication worker did not release its keepalive");
	}
}
