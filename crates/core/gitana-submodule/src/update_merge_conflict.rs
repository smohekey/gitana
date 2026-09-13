use std::fmt;

use crate::SubmoduleObjectId;

/// A merge update that left a module worktree in a conflict state.
#[derive(Debug)]
pub struct UpdateMergeConflict {
	pub name: String,
	pub path: String,
	pub target: SubmoduleObjectId,
	pub paths: Vec<String>,
}

impl fmt::Display for UpdateMergeConflict {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"automatic merge failed in submodule '{}'",
			self.name
		)
	}
}
