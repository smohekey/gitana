/// What deinit changed for one selected submodule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeinitOutcome {
	pub name: String,
	pub path: String,
	pub cleared: bool,
	pub unregistered: bool,
}
