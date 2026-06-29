//! Library face of the `gta` CLI. The command implementations live in `gta-core`; this crate is
//! the positional, git-like front-end (the binary in `main.rs`). The library target exists so the
//! clap command tree can be introspected by the `gta`/`gta-mcp` surface-parity test; it is not used
//! by the binary itself.

mod cli;

use clap::CommandFactory;

/// Parse the command line and run the requested command (the binary's entry point).
pub use self::cli::run;

/// The full clap command tree for `gta`. Used by the surface-parity test to assert that `gta` and
/// `gta-mcp` expose the same subcommands and arguments.
pub fn clap_command() -> clap::Command {
	self::cli::Cli::command()
}
