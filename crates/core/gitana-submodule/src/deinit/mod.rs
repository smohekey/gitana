mod deinit_failure;
#[cfg(not(target_arch = "wasm32"))]
mod deinit_mount_marker;
mod deinit_outcome;
mod deinit_report;
mod deinit_request;
#[cfg(not(target_arch = "wasm32"))]
mod durable_identity;

pub use self::deinit_failure::DeinitFailure;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::deinit_mount_marker::DeinitMountMarker;
pub use self::deinit_outcome::DeinitOutcome;
pub use self::deinit_report::DeinitReport;
pub use self::deinit_request::{DeinitRequest, DeinitSelection};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::durable_identity::DurableIdentity;
