//! `gta-core` — the command implementations behind the `gta` CLI and the `gta-mcp` MCP
//! server. The two front-ends each parse their own argument surface (positional/git-like
//! for `gta`, named for `gta-mcp`) and call into the `commands` here, which drive the
//! gitana engine and working tree in-process and print their results to stdout.

mod clone_destination;
mod command_context;
pub mod commands;
mod credential;
mod dispatch;
mod error;
mod excludes;
mod git_config;
mod git_path;
mod http_headers;
mod identity;
mod prompt;
mod repo;
mod repository_layout_identity;
mod retained_command_directory;
mod shallow;
mod signer;
mod ssh;
mod submodule_configuration;
mod submodule_transfer;
mod submodule_update_strategy;
mod url_rewrite;

pub(crate) use clone_destination::CloneDestination;
pub use command_context::CommandContext;
pub use credential::{CliCredentialProvider, transport_for};
pub use error::{AddAdvisory, MergeConflict, SilentExit};
pub use git_config::{validate_command_config, with_command_config, with_command_cwd};
pub use git_path::{bytes_from_os, pathspec_from_os, render_error};
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
pub use gitana_worktree::LsFilesOptions;
pub use prompt::with_terminal_prompts_disabled;
pub(crate) use repository_layout_identity::RepositoryLayoutIdentity;
pub(crate) use retained_command_directory::RetainedCommandDirectory;

/// How a frontend chooses pathname quoting for human-readable Git output.
#[derive(Clone, Copy)]
pub enum PathQuoteMode {
	/// Honour the repository's effective `core.quotePath` setting.
	Config,
	/// Always emit reversible C-style quoting for unsafe and non-UTF-8 bytes.
	Always,
}

/// How a frontend renders pathname values embedded in command result messages.
#[derive(Clone, Copy)]
pub enum ResultPathMode {
	/// Preserve valid UTF-8 for direct, human-facing CLI output.
	Human,
	/// Always emit reversible C-style quoting for machine consumption.
	Reversible,
}

/// The local file-store backend every command operates over: a [`WorktreeFileStore`], which routes
/// git's per-worktree files and shared files to the right directory so commands work the same in
/// an ordinary checkout and in a linked worktree (`git worktree add`).
pub(crate) type Backend = WorktreeFileStore;

/// The working-tree filesystem capability every command's working tree is served by: the native
/// cap-std [`CapWorkDir`], opened from the discovered work-tree path at the program edge.
pub(crate) type WorkDir = CapWorkDir;

/// The object-id type the CLI works with where no repository is in scope to read the hash
/// format from (e.g. `hash-object` outside a repo). Every command that opens a repository
/// instead routes through the runtime hash dispatch (see the `dispatch` module).
pub type Oid = gitana_object::ObjectId<gitana_object::Sha256>;
