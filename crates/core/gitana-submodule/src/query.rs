/// Path selection for a submodule operation.
#[derive(Clone, Debug, Default)]
pub struct SubmoduleQuery {
	pub pathspecs: Vec<String>,
}

impl SubmoduleQuery {
	pub fn all() -> Self {
		Self::default()
	}

	pub fn paths(pathspecs: Vec<String>) -> Self {
		Self { pathspecs }
	}
}
