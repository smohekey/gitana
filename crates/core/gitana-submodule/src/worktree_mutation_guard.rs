use std::path::Path;

use cap_std::fs::Dir;

use crate::{SubmoduleError, SubmoduleMutationLease, UpdateLockGuard, acquire_update_lock};

/// A stable, identity-checked serialization guard for one Gitana worktree mutation.
///
/// The underlying on-disk name remains the historical submodule-update lock, shared with init,
/// update, and deinit. General history operations use this wrapper so they cannot alter the index or
/// worktree while a retained merge is waiting to publish its captured `HEAD` transaction.
pub struct WorktreeMutationGuard {
	inner: UpdateLockGuard,
}

impl WorktreeMutationGuard {
	/// Retain this guard through a detached or blocking filesystem worker.
	pub fn lease(&self) -> SubmoduleMutationLease {
		self.inner.lease()
	}

	/// Revalidate the visible named guard before reporting an operation result.
	pub fn validate(&self) -> Result<(), SubmoduleError> {
		self.inner.validate()
	}
}

/// Try to serialize a general history mutation with submodule update/deinit in this worktree.
pub fn acquire_worktree_mutation_guard(
	git: &Dir,
	git_dir: &Path,
) -> Result<WorktreeMutationGuard, SubmoduleError> {
	Ok(WorktreeMutationGuard {
		inner: acquire_update_lock(git, git_dir)?,
	})
}
