//! Reusable, native repository discovery: locate and describe a git repository's on-disk layout.
//!
//! Given a starting path, this crate finds the containing repository — an ordinary work tree, a linked
//! worktree (`git worktree add`), or a bare repository — and reports its [`RepositoryLayout`]: the
//! working-tree root (if any), the per-worktree git directory, and the shared common directory, all as
//! canonical absolute paths.
//!
//! It reads only the ambient filesystem (no `git` subprocess, no global/system config, no network) and
//! never mutates anything. This is deliberately *ambient* path logic, kept out of the capability-pure
//! repository engine and the WASM guest: it mints paths that a caller then opens as capabilities.
//!
//! Discovery is `async` (offloading its blocking filesystem work) and separates a genuine absence of a
//! repository from corrupt or inaccessible metadata — see [`DiscoveryError`].

mod discovery;
mod discovery_error;
mod repository_layout;

pub use self::discovery::{common_dir_of, discover, inspect_root, try_discover};
pub use self::discovery_error::DiscoveryError;
pub use self::repository_layout::RepositoryLayout;
