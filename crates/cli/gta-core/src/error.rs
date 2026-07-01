//! Typed command outcomes that the front-ends translate into exit codes / tool results, rather than
//! a library function deciding the process's fate itself.

use std::fmt;

/// A merge (or merge-like operation) could not complete automatically: an in-progress merge has been
/// materialised — work-tree conflict markers, a conflicted index, and `MERGE_HEAD`/`MERGE_MSG` — and
/// the conflicted paths reported on stdout. Front-ends surface this as a non-zero exit (`gta`) or a
/// tool error (`gta-mcp`), without treating it as an internal failure or terminating the process.
#[derive(Debug)]
pub struct MergeConflict;

impl fmt::Display for MergeConflict {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"Automatic merge failed; fix conflicts and then commit the result."
		)
	}
}

impl std::error::Error for MergeConflict {}

/// A command result that is a non-zero exit with no CLI output — git's convention for a false
/// predicate or an empty result (e.g. `merge-base --is-ancestor` returning false, or no common
/// ancestor). `gta` maps it to a failing exit code and prints nothing; `gta-mcp` surfaces `reason`
/// as the tool error. Returned instead of `std::process::exit`, so a long-lived `gta-mcp` server is
/// not terminated by a library function deciding the process's fate.
#[derive(Debug)]
pub struct SilentExit {
	/// A short explanation for structured front-ends (`gta-mcp`); `gta` ignores it and stays silent.
	pub reason: &'static str,
}

impl fmt::Display for SilentExit {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.reason)
	}
}

impl std::error::Error for SilentExit {}
