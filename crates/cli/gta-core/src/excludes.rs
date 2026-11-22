//! Resolving git's global excludes file (`core.excludesFile`) for the working-tree commands.
//!
//! The excludes file lives *outside* the working tree, so the sandboxed `gitana-worktree` crate cannot
//! read it; the frontend resolves its content here and passes it in (to `status`, `add`, `checkout`, …).
//! `core.ignoreCase` and `.git/info/exclude` are read inside the worktree crate — only this global file
//! needs frontend help. Shared by every command that does untracked / ignored detection.

use std::path::{Path, PathBuf};

use anyhow::Result;
use cap_std::fs::Dir;
use gitana_config::GitConfig;
use gitana_path::GitPath;

/// The content of git's standard excludes file for a command invoked at `cwd` with discovered `prefix`
/// (the `/`-joined path below the work-tree root), or `None` when there is none. Resolved from the
/// effective `config`: `core.excludesFile` if set (`~` expanded, a relative path resolved against the
/// work-tree root, an explicitly empty value DISABLES), else git's default `$XDG_CONFIG_HOME/git/ignore`
/// (falling back to `~/.config/git/ignore`). A missing file contributes nothing; a configured path that
/// is a *directory* is fatal, as it is for git.
pub(crate) async fn resolve_excludes_file(
	config: &GitConfig,
	cwd: &Path,
	prefix: &GitPath,
) -> Result<Option<Vec<u8>>> {
	let root = worktree_root(cwd, prefix).await;
	gitana_config_native::excludes_file(config, &root).await
}

/// Resolve excludes for an exact already-opened worktree. Relative configured paths are read
/// through that capability so renaming the worktree cannot redirect or strand the lookup.
pub(crate) async fn resolve_excludes_file_at(
	config: &GitConfig,
	worktree: Dir,
	worktree_root: &Path,
) -> Result<Option<Vec<u8>>> {
	gitana_config_native::excludes_file_at(config, worktree, worktree_root).await
}

/// Reject a `core.excludesFile` that is present but valueless, as git does; see
/// [`gitana_config_native::validate_excludes_file_setting`].
pub(crate) fn validate_excludes_file_setting(config: &GitConfig) -> Result<()> {
	gitana_config_native::validate_excludes_file_setting(config)
}

/// The working-tree root: the invocation directory `cwd` with the discovered `prefix` (its `/`-joined
/// path below the root) stripped. git resolves a relative `core.excludesFile` against this root, not the
/// current subdirectory. `cwd` is canonicalised first so a symlinked `-C` argument still yields the real
/// root the `prefix` was measured against. The canonicalisation is async (offloaded) so it never blocks
/// the runtime on a slow filesystem, as `docs/conventions.md` requires of async command paths.
async fn worktree_root(cwd: &Path, prefix: &GitPath) -> PathBuf {
	let cwd = tokio::fs::canonicalize(cwd)
		.await
		.unwrap_or_else(|_| cwd.to_owned());
	if prefix.is_root() {
		return cwd;
	}
	let mut root = cwd.as_path();
	for _ in prefix.components() {
		root = root.parent().unwrap_or(root);
	}
	root.to_owned()
}
