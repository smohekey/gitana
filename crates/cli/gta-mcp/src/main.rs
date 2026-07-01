//! `gta-mcp` — the MCP-server front-end for the gta toolchain. It exposes the same
//! commands as the `gta` CLI (implemented in `gta-core`) as MCP tools: run `--mcp` to
//! serve over stdio or `--mcp-http` to serve over HTTP. With no MCP flag it runs a single
//! command and exits, exactly like `gta`.

mod cli;

use std::process::ExitCode;

use clap_mcp::ParseOrServeMcp;

fn main() -> ExitCode {
	// Serve MCP when `--mcp` / `--mcp-http` is present (clap-mcp drives its own runtime);
	// otherwise route normal argv through native clap parsing and run the one command.
	// The `_preserve_cli` entry is required: the non-preserve path panics accessing the
	// `mcp-http` arg id when a command runs (clap-mcp 0.0.5 + the `http` feature). `execute`
	// runs the command on a current-thread runtime.
	let cli = cli::Cli::parse_or_serve_mcp_preserve_cli();
	match cli::execute(cli) {
		Ok(_) => ExitCode::SUCCESS,
		Err(error) => {
			if let Some(silent) = error.0.downcast_ref::<gta_core::SilentExit>() {
				// A false predicate / empty result (e.g. `merge-base --is-ancestor`, or no common
				// ancestor): git's non-zero-with-no-output. Print the bare reason (no `gta-mcp:`
				// prefix) so an MCP tool call — which re-invokes this binary and builds its error from
				// the child's stderr — surfaces "not an ancestor" / "no common ancestor" to the client.
				eprintln!("{}", silent.reason);
			} else {
				eprintln!("gta-mcp: {:#}", error.0);
			}
			ExitCode::FAILURE
		}
	}
}
