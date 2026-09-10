use super::SyncOutcome;

/// Completed URL synchronization outcomes in selection order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
	pub outcomes: Vec<SyncOutcome>,
}
