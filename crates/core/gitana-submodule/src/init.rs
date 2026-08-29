use crate::SubmoduleQuery;

/// A request to register selected submodules in the superproject config.
#[derive(Clone, Debug, Default)]
pub struct InitRequest {
	pub query: SubmoduleQuery,
}

/// The result for one selected submodule.
#[derive(Clone, PartialEq, Eq)]
pub struct InitOutcome {
	pub name: String,
	pub path: String,
	/// The credential-safe registered URL, when this invocation installed one.
	pub registered_url: Option<String>,
	pub activated: bool,
	/// Credential-bearing form retained only inside this process so `update --init` can perform the
	/// initial transfer without ever persisting or formatting it.
	pub(crate) credential_url: Option<String>,
}

/// A non-fatal compatibility notice produced while planning initialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitNotice {
	/// The named remote had no URL, so the superproject itself supplied the relative URL base.
	AuthoritativeSuperproject { missing_key: String },
}

/// Structured results from an atomic init operation.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct InitReport {
	pub outcomes: Vec<InitOutcome>,
	pub notices: Vec<InitNotice>,
}

impl std::fmt::Debug for InitOutcome {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("InitOutcome")
			.field("name", &self.name)
			.field("path", &self.path)
			.field("registered_url", &self.registered_url)
			.field("activated", &self.activated)
			.finish_non_exhaustive()
	}
}

impl std::fmt::Debug for InitReport {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("InitReport")
			.field("outcomes", &self.outcomes)
			.field("notices", &self.notices)
			.finish()
	}
}
