use std::fmt;

/// An exclusively-created path whose removal is owned by this guard.
///
/// A path lock is the cross-process primitive used by Git lock protocols: creating the path is the
/// acquisition, and dropping the guard removes it synchronously. The synchronous release is important
/// for cancellation safety because [`Drop`] cannot await an asynchronous delete.
#[must_use = "dropping the path lock releases it immediately"]
pub struct PathLock {
	release: Option<Box<dyn FnOnce() + Send>>,
}

impl PathLock {
	/// Build a held lock from its synchronous release action.
	///
	/// This constructor exists for [`FileStore`](crate::FileStore) implementations. Callers acquire
	/// guards through [`FileStore::try_lock_path`](crate::FileStore::try_lock_path).
	pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
		Self {
			release: Some(Box::new(release)),
		}
	}
}

impl fmt::Debug for PathLock {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PathLock").finish_non_exhaustive()
	}
}

impl Drop for PathLock {
	fn drop(&mut self) {
		if let Some(release) = self.release.take() {
			release();
		}
	}
}
