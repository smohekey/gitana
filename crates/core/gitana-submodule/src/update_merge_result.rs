use crate::UpdateMergeOutcome;

/// The result of asking a native frontend to merge one module target.
pub enum UpdateMergeResult {
	Completed(UpdateMergeOutcome),
	Conflict { paths: Vec<String> },
}
