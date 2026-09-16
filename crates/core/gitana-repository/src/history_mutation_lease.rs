use gitana_file_store::{FileStore, PathLock};

use crate::RepositoryError;

const HISTORY_LOCK: &str = "gitana-history.lock";
const LOCK_ATTEMPTS: usize = 50;

#[cfg(not(target_arch = "wasm32"))]
const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// Exclusive permission to publish or remove repository history roots.
///
/// The lease is intentionally opaque: dropping it releases the repository-wide history gate. A
/// caller that starts a cancellation-sensitive mutation must move the lease into the retained task
/// that performs that mutation.
#[must_use = "a held HistoryMutationLease excludes other repository history mutations"]
pub struct HistoryMutationLease {
	_lock: PathLock,
}

impl HistoryMutationLease {
	pub(crate) async fn acquire(files: &impl FileStore) -> Result<Self, RepositoryError> {
		for attempt in 0..LOCK_ATTEMPTS {
			if let Some(lock) = files.try_lock_path(HISTORY_LOCK).await? {
				return Ok(Self { _lock: lock });
			}
			if attempt + 1 < LOCK_ATTEMPTS {
				lock_backoff().await;
			}
		}

		Err(RepositoryError::HistoryLocked)
	}
}

#[cfg(not(target_arch = "wasm32"))]
async fn lock_backoff() {
	let _ = tokio::task::spawn_blocking(|| std::thread::sleep(LOCK_BACKOFF)).await;
}

#[cfg(target_arch = "wasm32")]
async fn lock_backoff() {
	use std::future::Future;
	use std::pin::Pin;
	use std::task::{Context, Poll};

	struct Yield(bool);

	impl Future for Yield {
		type Output = ();

		fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
			if self.0 {
				Poll::Ready(())
			} else {
				self.0 = true;
				cx.waker().wake_by_ref();
				Poll::Pending
			}
		}
	}

	Yield(false).await;
}
