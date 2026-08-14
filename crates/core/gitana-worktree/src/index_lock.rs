use std::sync::atomic::{AtomicBool, Ordering};

use gitana_file_store::FileStore;

/// Proof that the index lock (`index.lock`) is held: minted only by [`WorkTree::lock_index`] and consumed
/// by [`WorkTree::commit_index`] / [`WorkTree::release_index_lock`], so the index cannot be written without
/// first taking the lock.
///
/// It is also an RAII guard that keeps the lock consistent under **cancellation** (the future being dropped
/// — a timeout, a `select!` losing branch). `index.lock` guards the invariant *"the working tree matches the
/// index"*, so the guard releases it on drop **only while that invariant still holds** — i.e. before any
/// working-tree write. Once an operation calls [`mark_mutation_started`](Self::mark_mutation_started) (right
/// before its first worktree mutation), a cancellation leaves `index.lock` in place — **fail-closed** — so no
/// later command proceeds against a half-applied working tree (git leaves its lock on an interrupted checkout
/// the same way; worktree writes are not atomic, so a mid-mutation cancellation cannot be made clean, only
/// protected). A `Drop` cannot `await`, so the release goes through the store's synchronous
/// [`remove_lock_file_sync`]. Releasing pre-mutation still matters: it stops a cancellation between locking
/// and the first write from stranding the lock and wedging every later index write.
///
/// The *commit* window is handled separately: [`WorkTree::commit_index`] [`disarm`](Self::disarm)s this guard
/// and hands the release to [`FileStore::replace_and_release_lock`], which writes the index and removes
/// `index.lock` in one blocking step that outlives cancellation — so the lock is never released before the
/// (uncancellable) write lands. [`WorkTree::release_index_lock`] (the error path) removes the lock with a
/// single synchronous unlink — but, like `Drop`, only when mutation has not begun. A process *killed*
/// mid-operation still orphans the lock (manual removal required), exactly as git's `index.lock` does.
///
/// [`WorkTree::lock_index`]: crate::WorkTree::lock_index
/// [`WorkTree::commit_index`]: crate::WorkTree::commit_index
/// [`WorkTree::release_index_lock`]: crate::WorkTree::release_index_lock
/// [`remove_lock_file_sync`]: gitana_file_store::FileStore::remove_lock_file_sync
/// [`FileStore::replace_and_release_lock`]: gitana_file_store::FileStore::replace_and_release_lock
pub(crate) struct IndexLock<'a, F: FileStore> {
	files: &'a F,
	released: bool,
	/// Set once the operation begins mutating the working tree, breaking the "worktree matches index"
	/// invariant. An [`AtomicBool`] (not a `Cell`) so the guard stays `Sync` and a future holding
	/// `&IndexLock` across an `.await` stays `Send`.
	mutation_started: AtomicBool,
}

impl<'a, F: FileStore> IndexLock<'a, F> {
	/// Mint a held-lock guard over `files` — called by `WorkTree::lock_index` once it has written
	/// `index.lock`. Starts armed (releases on cancellation) and pre-mutation.
	pub(crate) fn new(files: &'a F) -> Self {
		Self {
			files,
			released: false,
			mutation_started: AtomicBool::new(false),
		}
	}

	/// Signal that the working tree is now being mutated: a subsequent cancellation (or error release) must
	/// leave `index.lock` in place (fail-closed) rather than expose a half-applied working tree. Call once,
	/// immediately before the first worktree write; idempotent.
	pub(crate) fn mark_mutation_started(&self) {
		self.mutation_started.store(true, Ordering::Relaxed);
	}

	/// Whether the working tree has begun to be mutated (see [`mark_mutation_started`](Self::mark_mutation_started)).
	pub(crate) fn mutation_started(&self) -> bool {
		self.mutation_started.load(Ordering::Relaxed)
	}

	/// Disarm the `Drop` backstop: the async release path (in `commit_index` / `release_index_lock`) owns the
	/// on-disk unlink from here, so `Drop` must not also touch `index.lock`.
	pub(crate) fn disarm(&mut self) {
		self.released = true;
	}
}

impl<F: FileStore> Drop for IndexLock<'_, F> {
	fn drop(&mut self) {
		// Backstop for a cancelled future: remove `index.lock` so it is not stranded — but ONLY while the
		// working tree still matches the index (no mutation begun). Once mutation has started the tree is
		// half-applied, so leave the lock (fail-closed). A no-op once the async path already released it
		// (`released`), and best-effort besides — an unlink of an absent lock is a harmless ignored error.
		if !self.released && !self.mutation_started() {
			self.files.remove_lock_file_sync("index.lock");
		}
	}
}
