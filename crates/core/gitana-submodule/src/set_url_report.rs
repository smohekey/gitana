/// Result of changing and synchronizing one submodule URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetUrlReport {
	pub name: String,
	pub path: String,
	pub declaration_changed: bool,
	pub registration_synced: bool,
	pub module_remote: Option<String>,
	pub notices: Vec<crate::InitNotice>,
}
