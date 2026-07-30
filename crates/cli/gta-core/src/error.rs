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

/// `add` could not fully stage some pathspecs (out-of-cone and/or ignored) without extra flags: an
/// expected non-zero outcome carrying git's advisory, not an internal failure. The in-cone/tracked work
/// was already staged and saved by the engine; front-ends surface this as a non-zero exit (`gta` prints
/// the advisory to stderr without its `gta:` prefix — matching git — and `gta-mcp` as a tool error)
/// without terminating the process. `sparse` are the out-of-cone pathspecs (git's `--sparse` block, in
/// argument order); `ignored` are the reported ignored paths (git's `-f` block, sorted). Either or both
/// may be non-empty. `show_sparse_hints` / `show_ignored_hints` reflect `advice.updateSparsePath` /
/// `advice.addIgnoredFile` (git suppresses each block's `hint:` lines when its advice is false).
#[derive(Debug)]
pub struct AddAdvisory {
	sparse: Vec<String>,
	ignored: Vec<String>,
	show_sparse_hints: bool,
	show_ignored_hints: bool,
}

impl AddAdvisory {
	pub fn new(
		sparse: Vec<String>,
		ignored: Vec<String>,
		show_sparse_hints: bool,
		show_ignored_hints: bool,
	) -> Self {
		Self {
			sparse,
			ignored,
			show_sparse_hints,
			show_ignored_hints,
		}
	}
}

impl fmt::Display for AddAdvisory {
	/// git's advisory verbatim: the sparse block (if any) then the ignored block (if any), each with its
	/// header, one path per line, and — unless its advice config is false — its `hint:` lines. No blank
	/// line between the blocks and no trailing newline (the caller adds it). Probed vs git 2.50.1.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut lines: Vec<&str> = Vec::new();
		if !self.sparse.is_empty() {
			lines.push("The following paths and/or pathspecs matched paths that exist");
			lines.push("outside of your sparse-checkout definition, so will not be");
			lines.push("updated in the index:");
			lines.extend(self.sparse.iter().map(String::as_str));
			if self.show_sparse_hints {
				lines.push("hint: If you intend to update such entries, try one of the following:");
				lines.push("hint: * Use the --sparse option.");
				lines.push("hint: * Disable or modify the sparsity rules.");
				lines
					.push("hint: Disable this message with \"git config set advice.updateSparsePath false\"");
			}
		}
		if !self.ignored.is_empty() {
			lines.push("The following paths are ignored by one of your .gitignore files:");
			lines.extend(self.ignored.iter().map(String::as_str));
			if self.show_ignored_hints {
				lines.push("hint: Use -f if you really want to add them.");
				lines
					.push("hint: Disable this message with \"git config set advice.addIgnoredFile false\"");
			}
		}
		write!(f, "{}", lines.join("\n"))
	}
}

impl std::error::Error for AddAdvisory {}
