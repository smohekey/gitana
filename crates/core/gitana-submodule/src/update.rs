use std::fmt;

use crate::{InitReport, SubmoduleError, SubmoduleObjectId, SubmoduleQuery};

/// A one-level update request.
#[derive(Clone, Debug, Default)]
pub struct UpdateRequest {
	pub query: SubmoduleQuery,
	pub initialize: bool,
	/// Initialize only modules that are already active in the serialized effective configuration.
	/// Recursive clone enables this for its root level so command/global exclusions are not
	/// overwritten by ordinary per-module activation.
	pub initialize_only_active: bool,
	/// Reflog committer line supplied by the frontend; `None` disables module HEAD reflogs.
	pub reflog_committer: Option<String>,
}

/// What update did for one selected module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcomeState {
	SkippedUnregistered,
	SkippedInactive,
	SkippedByStrategy,
	AlreadyCurrent,
	Cloned,
	CheckedOut,
}

/// A structured per-module update result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateOutcome {
	pub name: String,
	pub path: String,
	pub recorded: SubmoduleObjectId,
	pub state: UpdateOutcomeState,
}

/// Completed outcomes in selection order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateReport {
	pub initialization: InitReport,
	pub outcomes: Vec<UpdateOutcome>,
}

/// An update failure that preserves the successfully completed prefix.
#[derive(Debug)]
pub struct UpdateFailure {
	pub completed: UpdateReport,
	pub module: Option<String>,
	pub source: SubmoduleError,
}

impl fmt::Display for UpdateFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(module) = &self.module {
			write!(formatter, "updating submodule '{module}': {}", self.source)
		} else {
			fmt::Display::fmt(&self.source, formatter)
		}
	}
}

impl std::error::Error for UpdateFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}
