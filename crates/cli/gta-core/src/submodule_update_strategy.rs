use std::path::PathBuf;

use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::MergeOutcome;
use gitana_submodule::{
	SubmoduleError, SubmoduleObjectId, UpdateMergeOutcome, UpdateMergeResult, UpdateStrategyExecutor,
};
use gitana_worktree::WorkTree;

use crate::git_path::render_result_path;
use crate::identity::CliIdentity;
use crate::signer;

/// Native history integration for submodule updates.
#[derive(Clone)]
pub(crate) struct SubmoduleUpdateStrategy {
	result_path_mode: crate::ResultPathMode,
	/// The environment override in force where the strategy was made, if any: its merges run on a
	/// task of their own, which doesn't inherit it.
	environment: Option<gitana_config_native::Environment>,
}

impl SubmoduleUpdateStrategy {
	/// The strategy writing paths as `result_path_mode` says, its merges under the environment
	/// override in force here, if there is one.
	pub(crate) fn new(result_path_mode: crate::ResultPathMode) -> Self {
		Self {
			result_path_mode,
			environment: gitana_config_native::current_environment(),
		}
	}
}

impl UpdateStrategyExecutor for SubmoduleUpdateStrategy {
	async fn merge<H: HashAlgorithm>(
		self,
		name: String,
		worktree: WorkTree<LocalFileStore, CapWorkDir, H>,
		worktree_root: PathBuf,
		worktree_directory: Dir,
		target: ObjectId<H>,
	) -> Result<UpdateMergeResult, SubmoduleError> {
		let environment = self.environment.clone();
		let merging = self.merging(name, worktree, worktree_root, worktree_directory, target);
		match environment {
			Some(environment) => gitana_config_native::with_environment(environment, merging).await,
			None => merging.await,
		}
	}
}

impl SubmoduleUpdateStrategy {
	async fn merging<H: HashAlgorithm>(
		self,
		_name: String,
		worktree: WorkTree<LocalFileStore, CapWorkDir, H>,
		worktree_root: PathBuf,
		worktree_directory: Dir,
		target: ObjectId<H>,
	) -> Result<UpdateMergeResult, SubmoduleError> {
		let identity = CliIdentity::new(worktree.repository());
		let signer =
			signer::config_signer_in(worktree.repository(), &worktree_root, &worktree_directory)
				.await
				.map_err(|error| SubmoduleError::Merge(error.to_string()))?;
		let outcome = gitana_porcelain::merge_with_excludes_loader(
			&worktree,
			&target.to_hex(),
			|| async {
				let config = worktree.repository().effective_config().await?;
				crate::excludes::resolve_excludes_file_at(
					&config,
					worktree_directory.try_clone()?,
					&worktree_root,
				)
				.await
			},
			false,
			false,
			&identity,
			signer.as_ref(),
		)
		.await
		.map_err(|error| SubmoduleError::Merge(error.to_string()))?;
		Ok(match outcome {
			MergeOutcome::AlreadyUpToDate => {
				UpdateMergeResult::Completed(UpdateMergeOutcome::AlreadyUpToDate)
			}
			MergeOutcome::FastForward { from, to } => {
				UpdateMergeResult::Completed(UpdateMergeOutcome::FastForward {
					from: from.map(SubmoduleObjectId::from_typed),
					to: SubmoduleObjectId::from_typed(to),
				})
			}
			MergeOutcome::Made { commit } => UpdateMergeResult::Completed(UpdateMergeOutcome::Made {
				commit: SubmoduleObjectId::from_typed(commit),
			}),
			MergeOutcome::WouldOverwrite { paths } => {
				return Err(SubmoduleError::Merge(
					gitana_porcelain::conflict::ConflictOverwriteError::new(paths)
						.render_with_paths(|path| render_result_path(path, self.result_path_mode)),
				));
			}
			MergeOutcome::Conflict { paths } => UpdateMergeResult::Conflict { paths },
		})
	}
}
