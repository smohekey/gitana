use std::marker::PhantomData;

use gitana_file_store::{FileStore, PathLock};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{RefStore, ReflogIntent, RepositoryError};

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
	lock: PathLock,
	_hash: PhantomData<H>,
}

impl<S, H> HeadLock<S, H>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	/// Wrap an already-acquired `HEAD.lock` with the owned store handle that will publish it.
	pub(crate) fn new(files: S, effective: Option<gitana_config::GitConfig>, lock: PathLock) -> Self {
		Self {
			files,
			effective,
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
		let HeadLock {
			files,
			effective,
			lock,
			_hash,
		} = self;
		let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
		store
			.commit_checkout(lock, branch, create, checkout_reflog)
			.await
	}
}
