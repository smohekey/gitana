//! `gta-core` — the command implementations behind the `gta` CLI and the `gta-mcp` MCP
//! server. The two front-ends each parse their own argument surface (positional/git-like
//! for `gta`, named for `gta-mcp`) and call into the `commands` here, which drive the
//! gitana engine and working tree in-process and print their results to stdout.

pub mod commands;
mod error;
mod identity;
mod remote;
mod repo;
mod transport;

pub use error::MergeConflict;
