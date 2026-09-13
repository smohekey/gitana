use std::future::Future;
use std::path::PathBuf;

use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_worktree::WorkTree;

use crate::{SubmoduleError, UpdateMergeResult};

/// Frontend capability for applying history-changing update strategies.
///
/// The submodule state machine retains the module repository, worktree, and serialization leases;
/// the frontend supplies identity, signing, and the porcelain merge implementation.
pub trait UpdateStrategyExecutor: Clone + Send + Sync + 'static {
	fn merge<H: HashAlgorithm>(
		self,
		name: String,
		worktree: WorkTree<LocalFileStore, CapWorkDir, H>,
		worktree_root: PathBuf,
		worktree_directory: Dir,
		target: ObjectId<H>,
	) -> impl Future<Output = Result<UpdateMergeResult, SubmoduleError>> + Send;
}

impl UpdateStrategyExecutor for () {
	async fn merge<H: HashAlgorithm>(
		self,
		name: String,
		_worktree: WorkTree<LocalFileStore, CapWorkDir, H>,
		_worktree_root: PathBuf,
		_worktree_directory: Dir,
		_target: ObjectId<H>,
	) -> Result<UpdateMergeResult, SubmoduleError> {
		Err(SubmoduleError::UnsupportedStrategy {
			name,
			strategy: "merge".to_owned(),
		})
	}
}
