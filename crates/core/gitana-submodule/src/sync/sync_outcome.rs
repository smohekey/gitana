use super::SyncOutcomeState;
use crate::InitNotice;

/// Structured URL synchronization result for one selected submodule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
	pub name: String,
	pub path: String,
	pub state: SyncOutcomeState,
	/// The attached module remote that was synchronized, when there was one.
	pub module_remote: Option<String>,
	pub notices: Vec<InitNotice>,
}
