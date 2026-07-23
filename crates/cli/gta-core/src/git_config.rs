//! Native discovery of git's configuration files and their precedence.
//!
//! The machinery now lives in the shared [`gitana_config_native`] crate so both the CLI and Code
//! Henge resolve credentials against git's real config stack the same way. This module re-exports it
//! under the historical `git_config` path the commands already reference — `from_repo` /
//! `from_ambient` for the merged read, `ConfigScope` / `write_path` / `read_file` / `write_file` for
//! the scoped `gta config` operations, `with_command_cwd` / `command_cwd` for the `-C` task-local, and
//! `parse_git_bool` for the credential and prompt code.

pub use gitana_config_native::*;
