use serde::{Deserialize, Serialize};

use super::DurableIdentity;

/// The exact ownership marker accepted for one mounted checkout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeinitMountMarker {
	pub(crate) identity: DurableIdentity,
	pub(crate) bytes: Vec<u8>,
}
