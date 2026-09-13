use crate::UpdateMergeOutcome;
use gitana_path::GitPath;

/// The result of asking a native frontend to merge one module target.
pub enum UpdateMergeResult {
	Completed(UpdateMergeOutcome),
	Conflict { paths: Vec<GitPath> },
}
