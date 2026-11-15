use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HeadTransaction, HistoryMutationLease, Repository, RepositoryError};

/// A reset prepared by [`Repository::prepare_reset_abandoning_operations`]: the history lease and
/// `HEAD`'s ref locks held, a merge's autostash already saved as the newest stash entry, and
/// `HEAD`'s move validated. Change the index and working tree while holding it, then
/// [`finish`](Self::finish) it to move `HEAD` and abandon the operation in progress. Dropped
/// unfinished, it releases everything with `HEAD` where it was; a saved autostash is then in both
/// `MERGE_AUTOSTASH` and the stash list, nothing lost.
#[must_use = "a PreparedReset holds the history lease and HEAD's ref locks until it is finished or dropped"]
pub struct PreparedReset<'a, F: FileStore, H: HashAlgorithm> {
	pub(crate) repository: &'a Repository<F, H>,
	pub(crate) transaction: HeadTransaction<F::Shared, H>,
	pub(crate) history_lease: HistoryMutationLease,
	pub(crate) autostash: Option<ObjectId<H>>,
}

impl<F, H> PreparedReset<'_, F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	/// The merge's autostash, saved as the newest stash entry, if there was one.
	pub fn autostash(&self) -> Option<ObjectId<H>> {
		self.autostash
	}

	/// Move `HEAD` as prepared, then abandon any merge, cherry-pick or revert in progress, as git's
	/// reset does: `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD` and `MERGE_MSG` removed, with
	/// git's `MERGE_MODE`, `AUTO_MERGE`, `SQUASH_MSG` and the saved `MERGE_AUTOSTASH`. A rebase's
	/// state is left to its own abort. Under the same lease, in one task that finishes even if this
	/// future is dropped. The autostash saved, if there was one.
	pub async fn finish(self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		let Self {
			repository,
			transaction,
			history_lease,
			autostash,
		} = self;
		#[cfg(not(target_arch = "wasm32"))]
		{
			let shared = repository.shared_repository();
			crate::retained_repository_task(async move {
				let _history_lease = history_lease;
				finish_reset(&shared, transaction, autostash).await
			})
			.await
		}
		#[cfg(target_arch = "wasm32")]
		{
			let _history_lease = history_lease;
			finish_reset(repository, transaction, autostash).await
		}
	}
}

/// [`PreparedReset::finish`], run where it's called.
async fn finish_reset<F, S, H>(
	repository: &Repository<F, H>,
	transaction: HeadTransaction<S, H>,
	autostash: Option<ObjectId<H>>,
) -> Result<Option<ObjectId<H>>, RepositoryError>
where
	F: FileStore,
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	transaction.finish().await?;
	if autostash.is_some() {
		crate::merge_state::clear_merge_autostash(repository).await?;
	}
	crate::merge_state::abandon_operations(repository).await?;
	Ok(autostash)
}
