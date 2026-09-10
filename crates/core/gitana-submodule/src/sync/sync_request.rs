use crate::SubmoduleQuery;

/// A one-level submodule URL synchronization request.
#[derive(Clone, Debug, Default)]
pub struct SyncRequest {
	pub query: SubmoduleQuery,
}
