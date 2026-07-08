//! `gta-core` — the command implementations behind the `gta` CLI and the `gta-mcp` MCP
//! server. The two front-ends each parse their own argument surface (positional/git-like
//! for `gta`, named for `gta-mcp`) and call into the `commands` here, which drive the
//! gitana engine and working tree in-process and print their results to stdout.

pub mod commands;
mod dispatch;
mod error;
mod identity;
mod repo;
mod shallow;
mod signer;

pub use error::{MergeConflict, SilentExit};
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};

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
