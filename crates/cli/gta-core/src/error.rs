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
