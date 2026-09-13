//! Capability-scoped, structured consumer operations for Git submodules.
#![allow(async_fn_in_trait)]

fn git_path(path: &str) -> gitana_path::GitPath {
	gitana_path::GitPath::from_utf8(path).expect("validated UTF-8 submodule path")
}

mod config_identity;
mod config_transition;
mod config_views;
mod configuration_provider;
#[cfg(not(target_arch = "wasm32"))]
mod context;
mod declaration;
mod deinit;
mod deinit_config_publication;
mod deinit_config_target;
#[cfg(not(target_arch = "wasm32"))]
mod deinit_operation;
mod deinit_worktree_attachment;
mod error;
mod init;
mod marker_target_resolver;
mod object_id;
mod query;
mod relative_url;
#[cfg(not(target_arch = "wasm32"))]
mod remote_url;
mod set_branch;
mod set_branch_outcome;
mod set_url;
#[cfg(not(target_arch = "wasm32"))]
mod set_url_config_path;
#[cfg(not(target_arch = "wasm32"))]
mod set_url_configuration_provider;
#[cfg(not(target_arch = "wasm32"))]
mod set_url_operation;
mod set_url_report;
mod set_url_request;
#[cfg(not(target_arch = "wasm32"))]
mod set_url_value;
#[cfg(not(target_arch = "wasm32"))]
mod shared_config_guard;
mod status;
mod submodule_mutation_lease;
mod sync;
#[cfg(not(target_arch = "wasm32"))]
mod transfer;
mod update;
mod update_merge_conflict;
mod update_merge_outcome;
#[cfg(not(target_arch = "wasm32"))]
mod update_merge_result;
#[cfg(not(target_arch = "wasm32"))]
mod update_operation;
mod update_strategy;
#[cfg(not(target_arch = "wasm32"))]
mod update_strategy_executor;
#[cfg(not(target_arch = "wasm32"))]
mod update_target;
#[cfg(not(target_arch = "wasm32"))]
mod worktree_mutation_guard;

pub(crate) use self::config_identity::ConfigIdentity;
pub use self::declaration::SubmoduleDeclaration;
pub(crate) use self::declaration::mappings_by_path;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::declaration::{declarations_by_path, validate_name, validate_path};
pub use self::deinit::{
	DeinitFailure, DeinitOutcome, DeinitReport, DeinitRequest, DeinitSelection,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::deinit::{DeinitMountMarker, DurableIdentity};
#[cfg(not(target_arch = "wasm32"))]
pub use self::deinit_operation::{
	deinit_recovery_git_dirs, pending_deinit_configs_require_restore, repository_has_pending_deinit,
	restore_pending_deinit_configs,
};
pub use self::marker_target_resolver::MarkerTargetResolver;
pub use config_transition::DeinitConfigTransition;
pub use config_views::ConfigViews;
pub use configuration_provider::{ConfigurationProvider, InitConfigResult, InitConfigUpdate};
#[cfg(not(target_arch = "wasm32"))]
pub use context::SubmoduleContext;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use context::is_active;
pub use deinit_config_publication::DeinitConfigPublication;
pub use deinit_config_target::DeinitConfigTarget;
pub use deinit_worktree_attachment::DeinitWorktreeAttachment;
pub use error::SubmoduleError;
pub use init::{InitNotice, InitOutcome, InitReport, InitRequest};
pub use object_id::SubmoduleObjectId;
pub use query::SubmoduleQuery;
pub use relative_url::{RelativeUrlError, resolve_relative_url};
pub use set_branch::set_branch;
pub use set_branch_outcome::SetBranchOutcome;
pub use set_url::set_url;
#[cfg(not(target_arch = "wasm32"))]
pub use set_url_config_path::SetUrlConfigPath;
#[cfg(not(target_arch = "wasm32"))]
pub use set_url_configuration_provider::SetUrlConfigurationProvider;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use set_url_operation::repository_has_pending_set_url_recovery;
#[cfg(not(target_arch = "wasm32"))]
pub use set_url_operation::{
	pending_set_url_configs_require_restore, repository_has_pending_set_url,
	repository_has_set_url_participant_claim, restore_pending_set_url_configs,
};
pub use set_url_report::SetUrlReport;
pub use set_url_request::SetUrlRequest;
#[cfg(not(target_arch = "wasm32"))]
pub use set_url_value::SetUrlValue;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use shared_config_guard::SharedConfigGuard;
pub use status::{SubmoduleStatus, SubmoduleStatusState};
pub use submodule_mutation_lease::SubmoduleMutationLease;
pub use sync::{SyncFailure, SyncOutcome, SyncOutcomeState, SyncReport, SyncRequest};
#[cfg(not(target_arch = "wasm32"))]
pub use transfer::{
	FetchRepository, FetchSource, FetchedTransfer, PrepareRepository, PrepareSource,
	PreparedTransfer, RepositoryTransfer,
};
pub use update::{UpdateFailure, UpdateOutcome, UpdateOutcomeState, UpdateReport, UpdateRequest};
pub use update_merge_conflict::UpdateMergeConflict;
pub use update_merge_outcome::UpdateMergeOutcome;
#[cfg(not(target_arch = "wasm32"))]
pub use update_merge_result::UpdateMergeResult;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use update_operation::{
	UpdateLockGuard, acquire_update_lock, acquire_update_lock_with_common, module_update_remote,
	try_acquire_submodule_config_mutation_lease,
};
#[cfg(not(target_arch = "wasm32"))]
pub use update_operation::{
	acquire_submodule_config_mutation_lease, acquire_submodule_config_setup_lease,
	repository_has_pending_update, try_acquire_submodule_config_setup_lease,
};
pub use update_strategy::UpdateStrategy;
#[cfg(not(target_arch = "wasm32"))]
pub use update_strategy_executor::UpdateStrategyExecutor;
#[cfg(not(target_arch = "wasm32"))]
pub use update_target::SubmoduleUpdateTarget;
#[cfg(not(target_arch = "wasm32"))]
pub use worktree_mutation_guard::{WorktreeMutationGuard, acquire_worktree_mutation_guard};
