/// An explicit selection for a deinitialization request.
#[derive(Clone, Debug)]
pub enum DeinitSelection {
	/// Select every tracked submodule.
	All,
	/// Select tracked submodules matching the supplied pathspecs.
	Paths(Vec<String>),
}

/// A one-level deinitialization request.
#[derive(Clone, Debug)]
pub struct DeinitRequest {
	pub selection: DeinitSelection,
	pub force: bool,
}
