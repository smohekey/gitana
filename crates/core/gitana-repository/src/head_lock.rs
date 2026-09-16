use std::marker::PhantomData;

use gitana_file_store::{FileStore, PathLock};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HistoryMutationLease, RefStore, ReflogIntent, RepositoryError};

/// An acquired `HEAD.lock` paired with an owned handle to the store that holds it.
///
/// Returned by [`RefStore::lock_head`](crate::RefStore::lock_head). It is a typed checkout capability:
/// the only way to consume it is to publish `HEAD` (optionally creating a branch first), and it retains
/// the lock until publication finishes — on native targets even across caller cancellation, because the
/// commit runs in an owned task that also owns the lock. `switch` acquires it before reading the merge
/// base and holds it through the working-tree mutation, so the branch it is on cannot move under an
/// in-flight checkout, and the `HEAD` publish cannot interleave with a concurrent retarget.
#[must_use = "a held HeadLock keeps HEAD.lock; consume it by publishing HEAD, or drop it to release"]
pub struct HeadLock<S, H: HashAlgorithm> {
	files: S,
	effective: Option<gitana_config::GitConfig>,
	history_lease: HistoryMutationLease,
	lock: PathLock,
	_hash: PhantomData<H>,
}

/// A detached `HEAD` publication whose namespace, reflog policy, old value, and reflog destination
/// have already been validated while retaining `HEAD.lock`.
///
/// Checkout code prepares this capability before mutating the index or worktree, then consumes it
/// after checkout. Dropping it publishes nothing and releases the lock. On native targets
/// [`finish`](Self::finish) transfers the capability to an owned task so cancellation cannot expose
/// an unlocked, half-published `HEAD` update.
#[must_use = "a prepared detached HEAD retains HEAD.lock; finish it after checkout, or drop it"]
pub struct PreparedDetachedHead<S, H: HashAlgorithm> {
	files: S,
	effective: Option<gitana_config::GitConfig>,
	history_lease: HistoryMutationLease,
	lock: PathLock,
	target: ObjectId<H>,
	reflog_content: Option<Vec<u8>>,
	_hash: PhantomData<H>,
}

impl<S, H> HeadLock<S, H>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	/// Wrap an already-acquired `HEAD.lock` with the owned store handle that will publish it.
	pub(crate) fn new(
		files: S,
		effective: Option<gitana_config::GitConfig>,
		history_lease: HistoryMutationLease,
		lock: PathLock,
	) -> Self {
		Self {
			files,
			effective,
			history_lease,
			lock,
			_hash: PhantomData,
		}
	}

	/// Publish a checkout, consuming the lock: optionally create `branch` at `target` (git's
	/// `switch -c`), then point `HEAD` at it.
	///
	/// Both steps run in one owned task that owns the lock, so a cancelled `switch` cannot release
	/// `HEAD.lock` mid-publish. When `HEAD` already points at `branch` (an unborn branch being born) the
	/// create cascades into `logs/HEAD`; it is written under the held lock rather than by re-locking it.
	pub async fn finish_checkout(
		self,
		branch: &str,
		create: Option<(ObjectId<H>, ReflogIntent<'_>)>,
		checkout_reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let branch = branch.to_owned();
			let create = create.map(|(target, reflog)| (target, own_reflog(reflog)));
			let checkout_reflog = own_reflog(checkout_reflog);
			match tokio::spawn(async move {
				let create = create
					.as_ref()
					.map(|(target, reflog)| (*target, borrow_reflog(reflog)));
				finish_checkout_inline(self, &branch, create, borrow_reflog(&checkout_reflog)).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		finish_checkout_inline(self, branch, create, checkout_reflog).await
	}

	/// Publish a detached checkout at `target`, consuming the held `HEAD.lock`.
	pub async fn finish_detached(
		self,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		self.prepare_detached(target, reflog).await?.finish().await
	}

	/// Validate a detached checkout publication without writing it, retaining `HEAD.lock` in the
	/// returned capability. This lets callers reject deterministic HEAD/reflog failures before they
	/// mutate a worktree.
	pub async fn prepare_detached(
		self,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<PreparedDetachedHead<S, H>, RepositoryError> {
		let HeadLock {
			files,
			effective,
			history_lease,
			lock,
			_hash,
		} = self;
		let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
		let reflog_content = store.prepare_detached_checkout(target, reflog).await?;
		Ok(PreparedDetachedHead {
			files,
			effective,
			history_lease,
			lock,
			target,
			reflog_content,
			_hash: PhantomData,
		})
	}
}

impl<S, H> PreparedDetachedHead<S, H>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	/// Publish the already-prepared reflog and detached HEAD, consuming the retained lock.
	pub async fn finish(self) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			match tokio::spawn(async move { finish_detached_inline(self).await }).await {
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		finish_detached_inline(self).await
	}
}

async fn finish_checkout_inline<S, H>(
	head: HeadLock<S, H>,
	branch: &str,
	create: Option<(ObjectId<H>, ReflogIntent<'_>)>,
	checkout_reflog: ReflogIntent<'_>,
) -> Result<(), RepositoryError>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	let HeadLock {
		files,
		effective,
		history_lease,
		lock,
		_hash,
	} = head;
	let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
	let result = store
		.commit_checkout(lock, branch, create, checkout_reflog)
		.await;
	drop(history_lease);
	result
}

async fn finish_detached_inline<S, H>(
	prepared: PreparedDetachedHead<S, H>,
) -> Result<(), RepositoryError>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	let PreparedDetachedHead {
		files,
		effective,
		history_lease,
		lock,
		target,
		reflog_content,
		_hash,
	} = prepared;
	let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
	let result = store
		.commit_prepared_detached_checkout(lock, target, reflog_content)
		.await;
	drop(history_lease);
	result
}

#[cfg(not(target_arch = "wasm32"))]
fn own_reflog(reflog: ReflogIntent<'_>) -> Option<(String, String)> {
	match reflog {
		ReflogIntent::Log { committer, message } => Some((committer.to_owned(), message.to_owned())),
		ReflogIntent::Skip => None,
	}
}

#[cfg(not(target_arch = "wasm32"))]
fn borrow_reflog(reflog: &Option<(String, String)>) -> ReflogIntent<'_> {
	match reflog {
		Some((committer, message)) => ReflogIntent::Log { committer, message },
		None => ReflogIntent::Skip,
	}
}
