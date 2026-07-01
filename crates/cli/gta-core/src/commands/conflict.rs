//! CLI-side conflict rendering. The conflict *lifecycle* (detecting an in-progress operation,
//! materialising/restoring state, capturing the resolved tree) now lives in
//! [`gitana_porcelain::conflict`]; the history-editing commands not yet migrated reach it through the
//! re-exports here. This module owns only what is CLI policy: printing the `CONFLICT` lines and
//! turning the conflict into the process's exit.

pub(crate) use gitana_porcelain::conflict::{
	ensure_trailing_newline, operation_in_progress, resolved_tree, restore_to_head,
	write_conflicted_state,
};

/// Report the conflicted paths on stdout and return the typed [`crate::MergeConflict`] outcome. The
/// front-end turns it into a non-zero exit (`gta`) or a tool error (`gta-mcp`); a library function
/// must not decide the process's fate with `exit`, which would terminate a long-lived MCP server.
pub(crate) fn report_conflicts(conflicts: &[String]) -> anyhow::Error {
	for path in conflicts {
		println!("CONFLICT (content): Merge conflict in {path}");
	}
	crate::MergeConflict.into()
}
