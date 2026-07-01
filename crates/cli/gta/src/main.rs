//! `gta` — a git-compatible, SHA-256 command-line tool driving the gitana engine
//! and working tree in-process on a local repository (see docs/hlds/gta-cli.md).
//!
//! The command implementations live in the `gta-core` library, shared with the
//! `gta-mcp` MCP server; this binary is the positional, git-like CLI front-end.

mod cli;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
	match cli::run().await {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			if error.downcast_ref::<gta_core::SilentExit>().is_some() {
				// A false predicate / empty result (e.g. `merge-base --is-ancestor`, or no common
				// ancestor): git's non-zero-with-no-output convention, not an error. Print nothing.
			} else if let Some(conflict) = error.downcast_ref::<gta_core::MergeConflict>() {
				// A materialised merge conflict is an expected non-zero outcome, not an internal error:
				// the conflicts were already reported, so print git's summary without the `gta:` prefix.
				println!("{conflict}");
			} else {
				eprintln!("gta: {error:#}");
			}
			ExitCode::FAILURE
		}
	}
}
