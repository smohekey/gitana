//! Resolving git's global excludes file (`core.excludesFile`) for the working-tree commands.
//!
//! The excludes file lives *outside* the working tree, so the sandboxed `gitana-worktree` crate cannot
//! read it; the frontend resolves its content here and passes it in (to `status`, `add`, `checkout`, …).
//! `core.ignoreCase` and `.git/info/exclude` are read inside the worktree crate — only this global file
//! needs frontend help. Shared by every command that does untracked / ignored detection.

use std::path::{Path, PathBuf};

use anyhow::Result;
use gitana_config::GitConfig;

/// The content of git's standard excludes file for a command invoked at `cwd` with discovered `prefix`
/// (the `/`-joined path below the work-tree root), or `None` when there is none. Resolved from the
/// effective `config`: `core.excludesFile` if set (`~` expanded, a relative path resolved against the
/// work-tree root, an explicitly empty value DISABLES), else git's default `$XDG_CONFIG_HOME/git/ignore`
/// (falling back to `~/.config/git/ignore`). A missing file contributes nothing; a configured path that
/// is a *directory* is fatal, as it is for git.
pub(crate) async fn resolve_excludes_file(
	config: &GitConfig,
	cwd: &Path,
	prefix: &str,
) -> Result<Option<String>> {
	validate_excludes_file_setting(config)?;
	let root = worktree_root(cwd, prefix).await;
	read_excludes_file(config.get_string("core", None, "excludesfile"), &root).await
}

/// Reject a `core.excludesFile` that is present but *valueless* (`[core]\n\texcludesFile`), which git
/// aborts with "missing value for 'core.excludesfile'" — a distinct case from a genuinely absent key,
/// which selects the XDG default. `get_string` collapses both to `None`, so the raw entry is inspected.
/// Validated by every command that would consult the excludes file, including `add -f`, which then skips
/// the filesystem read but still fails on this malformed setting, as git does.
pub(crate) fn validate_excludes_file_setting(config: &GitConfig) -> Result<()> {
	// git validates *every* occurrence and aborts on any valueless one, even when a later value shadows it,
	// so inspect all raw entries rather than just the winning value.
	if config
		.get_all_raw("core", None, "excludesfile")
		.iter()
		.any(Option::is_none)
	{
		anyhow::bail!("missing value for 'core.excludesfile'");
	}
	Ok(())
}

/// The working-tree root: the invocation directory `cwd` with the discovered `prefix` (its `/`-joined
/// path below the root) stripped. git resolves a relative `core.excludesFile` against this root, not the
/// current subdirectory. `cwd` is canonicalised first so a symlinked `-C` argument still yields the real
/// root the `prefix` was measured against. The canonicalisation is async (offloaded) so it never blocks
/// the runtime on a slow filesystem, as `docs/conventions.md` requires of async command paths.
async fn worktree_root(cwd: &Path, prefix: &str) -> PathBuf {
	let cwd = tokio::fs::canonicalize(cwd)
		.await
		.unwrap_or_else(|_| cwd.to_owned());
	let prefix = prefix.trim_matches('/');
	if prefix.is_empty() {
		return cwd;
	}
	let mut root = cwd.as_path();
	for _ in prefix.split('/') {
		root = root.parent().unwrap_or(root);
	}
	root.to_owned()
}

/// The content of git's standard excludes file, or `None`. An explicitly empty `core.excludesFile`
/// disables it (git's way); a set value is used (with `~` expansion, and a relative path resolved
/// against the worktree `root`); when unset, git's default `$XDG_CONFIG_HOME/git/ignore` (falling back
/// to `~/.config/git/ignore`). A missing file contributes no patterns, but any other read failure — a
/// configured path that is a directory or is unreadable — is an error, as it is for git. The filesystem
/// reads are async (offloaded) so a slow/network-mounted excludes file never blocks the runtime.
async fn read_excludes_file(configured: Option<&str>, root: &Path) -> Result<Option<String>> {
	let path = match configured {
		Some("") => return Ok(None), // explicitly disabled
		Some(value) => {
			let expanded = expand_tilde(value)?;
			// A bare `~` under an empty `HOME` expands to the empty path: git treats that as no excludes file
			// and continues (probed vs git 2.55: `core.excludesFile=~` with `HOME=` exits 0, adds no patterns),
			// rather than resolving it against the worktree root and rejecting the root directory. (`~/` still
			// expands to the absolute `/`, which git — and the directory check below — do reject.)
			if expanded.as_os_str().is_empty() {
				return Ok(None);
			}
			if expanded.is_absolute() {
				expanded
			} else {
				root.join(expanded)
			}
		}
		None => match xdg_config_home() {
			Some(base) => base.join("git").join("ignore"),
			None => return Ok(None),
		},
	};
	match tokio::fs::read(&path).await {
		Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
		// A missing file is fine (a configured-but-absent path, or no XDG default, just adds no patterns).
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		// git is fatal ("cannot use … as an exclude file") only when the path is a *directory*; a regular
		// file that cannot be read (permission-denied) is a warning it continues past — so it just adds no
		// patterns here.
		Err(error) => {
			let is_dir = tokio::fs::metadata(&path)
				.await
				.map(|meta| meta.is_dir())
				.unwrap_or(false);
			if is_dir {
				Err(
					anyhow::Error::new(error)
						.context(format!("cannot use {} as an exclude file", path.display())),
				)
			} else {
				Ok(None)
			}
		}
	}
}

/// Expand a leading `~` / `~/…` against `$HOME` (git's excludes-file tilde handling); `~user` is not
/// supported and yields an error. A path without a leading `~` is returned as-is.
fn expand_tilde(path: &str) -> Result<PathBuf> {
	let Some(rest) = path.strip_prefix('~') else {
		return Ok(PathBuf::from(path));
	};
	if !rest.is_empty() && !rest.starts_with('/') {
		anyhow::bail!("unsupported `~user` in core.excludesFile: {path}");
	}
	// Only an *unset* `HOME` is fatal. Concatenate `HOME` with the remainder git-style (string append, not
	// path-join), keeping the leading separator: an empty `HOME` expands `~/foo` to the absolute `/foo` (and
	// bare `~` to `HOME` itself — the empty path, which the caller then treats as no excludes file, as git
	// does), rather than dropping the separator into a repository-relative path that could hide files.
	// (Unlike the XDG default path below, where an empty `HOME` is treated as unset.)
	let mut expanded = std::env::var_os("HOME")
		.ok_or_else(|| anyhow::anyhow!("cannot expand `~`: $HOME is not set"))?;
	expanded.push(rest);
	Ok(PathBuf::from(expanded))
}

/// git's XDG base for user config: `$XDG_CONFIG_HOME` when set and non-empty, else `~/.config`. A
/// *relative* `$XDG_CONFIG_HOME` is used as-is (resolved against the process directory), a documented
/// best-effort gap versus git, which resolves it against the repository — a pathological configuration.
/// An **empty** `HOME` (as sanitized/container environments often set) is treated as unset, so the
/// default excludes path is not silently resolved relative to the process directory (`.config/git/ignore`).
fn xdg_config_home() -> Option<PathBuf> {
	if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
		&& !xdg.is_empty()
	{
		return Some(PathBuf::from(xdg));
	}
	std::env::var_os("HOME")
		.filter(|home| !home.is_empty())
		.map(|home| PathBuf::from(home).join(".config"))
}
