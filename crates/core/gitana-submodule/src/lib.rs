//! Capability-scoped, structured consumer operations for Git submodules.
#![allow(async_fn_in_trait)]

mod config_views;
mod configuration_provider;
#[cfg(not(target_arch = "wasm32"))]
mod context;
mod declaration;
mod error;
mod init;
mod object_id;
mod query;
mod relative_url;
mod remote_url;
mod status;
#[cfg(not(target_arch = "wasm32"))]
mod transfer;
mod update;
#[cfg(not(target_arch = "wasm32"))]
mod update_operation;

pub use config_views::ConfigViews;
pub use configuration_provider::{ConfigurationProvider, InitConfigResult, InitConfigUpdate};
#[cfg(not(target_arch = "wasm32"))]
pub use context::SubmoduleContext;
pub use declaration::SubmoduleDeclaration;
pub use error::SubmoduleError;
pub use init::{InitNotice, InitOutcome, InitReport, InitRequest};
pub use object_id::SubmoduleObjectId;
pub use query::SubmoduleQuery;
pub use relative_url::{RelativeUrlError, resolve_relative_url};
pub use status::{SubmoduleStatus, SubmoduleStatusState};
#[cfg(not(target_arch = "wasm32"))]
pub use transfer::{
	FetchRepository, FetchSource, FetchedTransfer, PrepareRepository, PrepareSource,
	PreparedTransfer, RepositoryTransfer,
};
pub use update::{UpdateFailure, UpdateOutcome, UpdateOutcomeState, UpdateReport, UpdateRequest};
