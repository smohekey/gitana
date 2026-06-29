//! Library face of the `gta-mcp` CLI. The MCP server binary lives in `main.rs`; this library
//! target exists so the clap command tree can be introspected by the `gta`/`gta-mcp` surface-parity
//! test. It is not used by the binary itself.

mod cli;

use clap::CommandFactory;

/// The full clap command tree for `gta-mcp`. Used by the surface-parity test to assert that `gta`
/// and `gta-mcp` expose the same subcommands and arguments.
pub fn clap_command() -> clap::Command {
	self::cli::Cli::command()
}
