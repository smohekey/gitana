use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::HashAlgorithm;
use gitana_path::{GitPath, GitPathspec};

use crate::{Index, WorkTree, WorktreeError, index_lock::IndexLock};

/// A held `index.lock` transaction whose index snapshot was loaded only after
/// acquiring the lock.
///
/// Changes are accumulated in memory until [`publish`](Self::publish) writes
/// them while retaining the lock. [`finish`](Self::finish) accepts the
/// published index and releases the lock, while [`rollback`](Self::rollback)
/// restores the exact original index before releasing it. Dropping a
/// transaction before publication releases the lock; dropping it during or
/// after publication leaves the lock in place so an interrupted caller fails
/// closed.
pub struct IndexTransaction<'a, F: FileStore, W: WorkDirFs, H: HashAlgorithm> {
	worktree: &'a WorkTree<F, W, H>,
	lock: Option<IndexLock<'a, F>>,
	original: Index<H>,
	index: Index<H>,
	published: bool,
}

impl<'a, F: FileStore, W: WorkDirFs, H: HashAlgorithm> IndexTransaction<'a, F, W, H> {
	pub(crate) fn new(
		worktree: &'a WorkTree<F, W, H>,
		lock: IndexLock<'a, F>,
		index: Index<H>,
	) -> Self {
		Self {
			worktree,
			lock: Some(lock),
			original: index.clone(),
			index,
			published: false,
		}
	}

	/// The index loaded after `index.lock` was acquired.
	pub fn index(&self) -> &Index<H> {
		&self.index
	}

	/// Mutable access to the index protected by this transaction.
	pub fn index_mut(&mut self) -> &mut Index<H> {
		&mut self.index
	}

	/// Stage pathspecs into the transaction's in-memory index.
	pub async fn stage_pathspecs(
		&mut self,
		pathspecs: &[GitPathspec],
		prefix: &GitPath,
		force: bool,
		excludes_file: Option<&[u8]>,
	) -> Result<(), WorktreeError> {
		self
			.worktree
			.stage_pathspecs_into(&mut self.index, pathspecs, prefix, force, excludes_file)
			.await
	}

	/// Publish the current index while retaining `index.lock`.
	pub async fn publish(&mut self) -> Result<(), WorktreeError> {
		let lock = self
			.lock
			.as_ref()
			.expect("an active index transaction owns its lock");
		lock.mark_mutation_started();
		self
			.worktree
			.files()
			.write_path_replace("index", &self.index.write_v4())
			.await?;
		self.published = true;
		Ok(())
	}

	/// Keep the published index and release `index.lock`.
	pub fn finish(mut self) {
		let mut lock = self
			.lock
			.take()
			.expect("an active index transaction owns its lock");
		self.worktree.files().remove_lock_file_sync("index.lock");
		lock.disarm();
	}

	/// Restore the exact original index, then release `index.lock`.
	pub async fn rollback(mut self) -> Result<(), WorktreeError> {
		let mut lock = self
			.lock
			.take()
			.expect("an active index transaction owns its lock");
		if self.published || lock.mutation_started() {
			self
				.worktree
				.files()
				.write_path_replace("index", &self.original.write_v4())
				.await?;
		}
		self.worktree.files().remove_lock_file_sync("index.lock");
		lock.disarm();
		Ok(())
	}
}
