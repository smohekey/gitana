//! The per-repository worktree-registration lock — gitana's serializer for admin-dir create/remove.
//!
//! Requirement (Code Henge): a lost race on a worktree registration MUST be reported as a conflict, not
//! an overwrite. The branch ref is already CAS-safe via [`gitana_repository::RefStore`]; the gap is the
//! *registration* (the admin dir under `<common>/worktrees/`), whose name-selection + pointer writes have
//! no lock. This closes it with a single per-repository lock file held across the whole
//! inspect→decide→mutate critical section of both `create` and `remove`.
//!
//! It **mirrors the file store's `LockFileGuard`** (`gitana-file-store-local`): the lock is a file created
//! exclusively (`O_EXCL`), brief-retried on contention, then a structured
//! [`RegistrationLocked`](crate::LinkedWorktreeError::RegistrationLocked) error; the file is removed on
//! `Drop`, so a dropped (cancelled) future releases it. Deliberately **exceeds git's own guarantee** — git's
//! `worktree add`/`remove` share the same non-atomicity — and serializes gitana-vs-gitana operations; a
//! concurrent stock-git change is still *detected* by the immediate-pre-destroy re-inspect. A stale lock
//! (crashed process) is **not** auto-broken — it errors until the file is removed, exactly as gitana treats
//! a stale `<ref>.lock`.

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use std::path::{Path, PathBuf};
	use std::time::Duration;

	use crate::LinkedWorktreeError;

	/// Retries acquiring a contended lock, and the wait between tries — matches the file store's
	/// `LockFileGuard` (50 × 10 ms), so a worktree operation waits for a concurrent gitana (or a slow
	/// filesystem) about as long before surfacing the contention as a structured conflict.
	const LOCK_ATTEMPTS: usize = 50;
	const LOCK_BACKOFF: Duration = Duration::from_millis(10);

	/// A held registration lock. Its `Drop` removes the lock file, so the lock is released on any return
	/// **and on cancellation** (a dropped future). The lock file's mere existence *is* the lock — the open
	/// handle is dropped immediately, keeping the guard `Send` across the `.await`s it is held over.
	pub(crate) struct RegistrationLock {
		path: PathBuf,
	}

	impl RegistrationLock {
		/// Acquire `<common>/worktrees.lock` (a sibling of `worktrees/`, so it is never scanned as an admin
		/// entry). `<common>` always exists, so no parent is created. Retries briefly on contention, then
		/// returns [`LinkedWorktreeError::RegistrationLocked`].
		pub(crate) async fn acquire(common: &Path) -> Result<Self, LinkedWorktreeError> {
			let path = common.join("worktrees.lock");
			for attempt in 0..LOCK_ATTEMPTS {
				match std::fs::OpenOptions::new()
					.write(true)
					.create_new(true)
					.open(&path)
				{
					// The file now exists (and its handle is dropped here) — we hold the lock.
					Ok(_file) => return Ok(RegistrationLock { path }),
					Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
						// Don't sleep after the final attempt — fail promptly.
						if attempt + 1 < LOCK_ATTEMPTS {
							backoff().await;
						}
					}
					Err(e) => {
						return Err(LinkedWorktreeError::io(
							"acquiring registration lock",
							&path,
							e,
						));
					}
				}
			}
			Err(LinkedWorktreeError::RegistrationLocked(path))
		}
	}

	impl Drop for RegistrationLock {
		fn drop(&mut self) {
			// Best-effort, like the file store's guard: a failure to unlink leaves a stale lock (a rare
			// crash-class artifact) that the next operation surfaces as `RegistrationLocked`, never a silent
			// corruption.
			let _ = std::fs::remove_file(&self.path);
		}
	}

	/// Wait before retrying a contended lock. The sleep is **offloaded with `spawn_blocking` and awaited**
	/// (mirroring the file store's `lock_backoff`), so `.await` is a genuine suspension point: the executor
	/// stays free during the wait and a lock *holder* sharing the runtime keeps making progress toward
	/// releasing its guard. A plain `std::thread::sleep` here would block the executor thread and stall the
	/// holder — deadlocking a same-runtime waiter into a false `RegistrationLocked`.
	async fn backoff() {
		let _ = tokio::task::spawn_blocking(|| std::thread::sleep(LOCK_BACKOFF)).await;
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::RegistrationLock;
