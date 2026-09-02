use super::DeinitOutcome;

/// Completed deinitializations in selection order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeinitReport {
	pub outcomes: Vec<DeinitOutcome>,
}
