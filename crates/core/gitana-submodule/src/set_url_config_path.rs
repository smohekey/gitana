use std::path::Path;

/// Capability-relative and diagnostic paths for one set-URL config participant.
pub struct SetUrlConfigPath<'a> {
	/// Path interpreted relative to the supplied directory capability.
	pub relative: &'a Path,
	/// Path used only for diagnostics.
	pub display: &'a Path,
}
