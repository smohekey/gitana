//! CLI-side conflict rendering. The conflict *lifecycle* (detecting an in-progress operation,
//! materialising/restoring state, capturing the resolved tree) lives in [`gitana_porcelain::conflict`];
//! the history-editing commands there return the conflicted paths as data. This module owns only what
//! is CLI policy: printing the `CONFLICT` lines and turning the conflict into the process's exit.

/// Report the conflicted paths on stdout and return the typed [`crate::MergeConflict`] outcome. The
/// front-end turns it into a non-zero exit (`gta`) or a tool error (`gta-mcp`); a library function
/// must not decide the process's fate with `exit`, which would terminate a long-lived MCP server.
pub(crate) fn print_conflicts(conflicts: &[gitana_path::GitPath], mode: crate::ResultPathMode) {
	for line in conflict_lines(conflicts, mode) {
		println!("{line}");
	}
}

pub(crate) fn report_conflicts(
	conflicts: &[gitana_path::GitPath],
	mode: crate::ResultPathMode,
) -> anyhow::Error {
	print_conflicts(conflicts, mode);
	crate::MergeConflict.into()
}

fn conflict_lines(conflicts: &[gitana_path::GitPath], mode: crate::ResultPathMode) -> Vec<String> {
	conflicts
		.iter()
		.map(|path| {
			format!(
				"CONFLICT (content): Merge conflict in {}",
				crate::git_path::render_result_path(path, mode)
			)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reversible_conflict_lines_distinguish_human_collisions() {
		let raw = gitana_path::GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = gitana_path::GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let conflicts = [raw, literal];
		let human = conflict_lines(&conflicts, crate::ResultPathMode::Human);
		let reversible = conflict_lines(&conflicts, crate::ResultPathMode::Reversible);

		assert_eq!(human[0], human[1]);
		assert_ne!(reversible[0], reversible[1]);
	}
}
