//! Capability-scoped, structured consumer operations for Git submodules.
#![allow(async_fn_in_trait)]

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
#[cfg(not(target_arch = "wasm32"))]
mod shared_config_guard;
mod status;
mod submodule_mutation_lease;
#[cfg(not(target_arch = "wasm32"))]
mod transfer;
mod update;
#[cfg(not(target_arch = "wasm32"))]
mod update_operation;
#[cfg(not(target_arch = "wasm32"))]
mod update_target;

pub(crate) use self::config_identity::ConfigIdentity;
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
pub use declaration::SubmoduleDeclaration;
pub use deinit_config_publication::DeinitConfigPublication;
pub use deinit_config_target::DeinitConfigTarget;
pub use deinit_worktree_attachment::DeinitWorktreeAttachment;
pub use error::SubmoduleError;
pub use init::{InitNotice, InitOutcome, InitReport, InitRequest};
pub use object_id::SubmoduleObjectId;
pub use query::SubmoduleQuery;
pub use relative_url::{RelativeUrlError, resolve_relative_url};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use shared_config_guard::SharedConfigGuard;
pub use status::{SubmoduleStatus, SubmoduleStatusState};
pub use submodule_mutation_lease::SubmoduleMutationLease;
#[cfg(not(target_arch = "wasm32"))]
pub use transfer::{
	FetchRepository, FetchSource, FetchedTransfer, PrepareRepository, PrepareSource,
	PreparedTransfer, RepositoryTransfer,
};
pub use update::{UpdateFailure, UpdateOutcome, UpdateOutcomeState, UpdateReport, UpdateRequest};
#[cfg(not(target_arch = "wasm32"))]
pub use update_operation::{
	acquire_submodule_config_mutation_lease, acquire_submodule_config_setup_lease,
	repository_has_pending_update, try_acquire_submodule_config_setup_lease,
};
#[cfg(not(target_arch = "wasm32"))]
pub use update_target::SubmoduleUpdateTarget;
