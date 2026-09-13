use gitana_file_store::{FileStore, PathLock};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HeadResetPlan, HeadState, RefStore, ReflogIntent, RepositoryError};

/// A locked snapshot of `HEAD`, its symbolic chain, and the terminal direct ref.
///
/// Merge code retains this transaction from before it reads the starting tip through index,
/// worktree, and ref publication. A symbolic `HEAD`, every referent, `ORIG_HEAD`, and `packed-refs`
/// therefore cannot be changed by another conforming writer while the merge is in flight.
#[must_use = "a held HeadTransaction retains ref locks until it is finished or dropped"]
pub struct HeadTransaction<S, H: HashAlgorithm> {
	pub(crate) files: S,
	pub(crate) effective: Option<gitana_config::GitConfig>,
	pub(crate) locks: Vec<PathLock>,
	pub(crate) lock_names: Vec<String>,
	pub(crate) state: HeadState<H>,
	pub(crate) head_chain: Vec<String>,
	pub(crate) tip: Option<ObjectId<H>>,
	pub(crate) prepared: Option<HeadResetPlan<H>>,
}

impl<S, H> HeadTransaction<S, H>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	/// The exact `HEAD` state captured under the transaction's locks.
	pub fn state(&self) -> &HeadState<H> {
		&self.state
	}

	/// The captured commit, or `None` for an unborn symbolic branch.
	pub fn tip(&self) -> Option<ObjectId<H>> {
		self.tip
	}

	/// Validate and prepare a reset-style publication while retaining all locks.
	///
	/// Call this before changing the index or worktree. [`finish`](Self::finish) then publishes
	/// `ORIG_HEAD`, the captured terminal ref, and every reflog after revalidating the symbolic chain.
	pub async fn prepare_reset(
		&mut self,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let state = self.state.clone();
		let head_chain = self.head_chain.clone();
		let tip = self.tip;
		let plan = {
			let store = RefStore::<_, H>::new(&self.files).with_effective_config(self.effective.as_ref());
			store
				.prepare_head_reset(&state, &head_chain, tip, target, reflog)
				.await?
		};
		self.prepared = Some(plan);
		Ok(())
	}

	/// Publish the prepared reset and release the retained ref locks.
	///
	/// Native publication runs in an owned task, so dropping the awaiting future cannot release the
	/// locks while a backend write is still in progress.
	pub async fn finish(self) -> Result<(), RepositoryError> {
		if self.prepared.is_none() {
			return Err(RepositoryError::Unsupported(
				"HEAD transaction was not prepared for publication".to_owned(),
			));
		}

		#[cfg(not(target_arch = "wasm32"))]
		{
			match tokio::spawn(async move { finish_inline(self).await }).await {
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		finish_inline(self).await
	}

	/// Publish the captured starting tip to `ORIG_HEAD` without moving `HEAD`.
	///
	/// Conflict materialisation uses this after writing its index and worktree state. The operation
	/// consumes the transaction so the original symbolic chain remains locked and revalidated through
	/// `ORIG_HEAD` publication. If that chain itself terminates at `ORIG_HEAD`, publication is already
	/// satisfied and only the snapshot validation is required.
	pub async fn finish_orig_head(self) -> Result<(), RepositoryError> {
		if self.prepared.is_some() {
			return Err(RepositoryError::Unsupported(
				"HEAD transaction was prepared for reset publication".to_owned(),
			));
		}
		if self.tip.is_none() {
			return Err(RepositoryError::Unsupported(
				"cannot record ORIG_HEAD for an unborn HEAD".to_owned(),
			));
		}

		#[cfg(not(target_arch = "wasm32"))]
		{
			match tokio::spawn(async move { finish_orig_head_inline(self).await }).await {
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		finish_orig_head_inline(self).await
	}
}

async fn finish_inline<S, H>(mut transaction: HeadTransaction<S, H>) -> Result<(), RepositoryError>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	let state = transaction.state.clone();
	let head_chain = transaction.head_chain.clone();
	let tip = transaction.tip;
	let plan = transaction
		.prepared
		.clone()
		.expect("finish checks that the HEAD transaction is prepared");
	let result = {
		let store = RefStore::<_, H>::new(&transaction.files)
			.with_effective_config(transaction.effective.as_ref());
		store
			.commit_head_reset(&state, &head_chain, tip, &plan)
			.await
	};
	let names = std::mem::take(&mut transaction.lock_names);
	transaction.locks.clear();
	let store =
		RefStore::<_, H>::new(&transaction.files).with_effective_config(transaction.effective.as_ref());
	for name in names {
		store.prune_lock_parents(&name).await;
	}
	result
}

async fn finish_orig_head_inline<S, H>(
	mut transaction: HeadTransaction<S, H>,
) -> Result<(), RepositoryError>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	let result = {
		let store = RefStore::<_, H>::new(&transaction.files)
			.with_effective_config(transaction.effective.as_ref());
		store
			.commit_head_orig(
				&transaction.state,
				&transaction.head_chain,
				transaction
					.tip
					.expect("finish_orig_head checks for a born HEAD"),
			)
			.await
	};
	let names = std::mem::take(&mut transaction.lock_names);
	transaction.locks.clear();
	let store =
		RefStore::<_, H>::new(&transaction.files).with_effective_config(transaction.effective.as_ref());
	for name in names {
		store.prune_lock_parents(&name).await;
	}
	result
}
