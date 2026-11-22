//! git's global excludes file (`core.excludesFile`), found and read for the working-tree library,
//! which takes its content from the caller because it lives outside the working tree.

use std::path::{Path, PathBuf};

use anyhow::Result;
use cap_std::fs::Dir;
use gitana_config::GitConfig;

/// The content of git's standard excludes file for a worktree whose root folder is
/// `worktree_root`, or `None` when there is none. Resolved from the effective `config`:
/// `core.excludesFile` if set (`~` expanded, a relative path resolved against the work-tree root,
/// an explicitly empty value disabling it), else git's default `$XDG_CONFIG_HOME/git/ignore`
/// (falling back to `~/.config/git/ignore`). A missing file contributes nothing; a configured path
/// that is a folder is fatal, as it is for git; a valueless `core.excludesFile` is refused, as
/// [`validate_excludes_file_setting`] does.
pub async fn excludes_file(config: &GitConfig, worktree_root: &Path) -> Result<Option<Vec<u8>>> {
	validate_excludes_file_setting(config)?;
	read_excludes_file(
		config.get_string("core", None, "excludesfile"),
		worktree_root,
	)
	.await
}

/// [`excludes_file`] for a worktree already open as `worktree`, whose root folder is
/// `worktree_root`: a configured file inside the worktree is read through the open folder, so
/// renaming the worktree cannot redirect or strand the lookup.
pub async fn excludes_file_at(
	config: &GitConfig,
	worktree: Dir,
	worktree_root: &Path,
) -> Result<Option<Vec<u8>>> {
	validate_excludes_file_setting(config)?;
	let Some(configured) = config.get_string("core", None, "excludesfile") else {
		return read_excludes_file(None, worktree_root).await;
	};
	if configured.is_empty() {
		return Ok(None);
	}
	let expanded = expand_tilde(configured)?;
	if expanded.as_os_str().is_empty() {
		return Ok(None);
	}
	let (relative, display) = if expanded.is_absolute() {
		let Some(relative) = absolute_path_in_worktree(&expanded, worktree_root).await else {
			return read_excludes_file(Some(configured), worktree_root).await;
		};
		let relative = if relative.as_os_str().is_empty() {
			PathBuf::from(".")
		} else {
			relative
		};
		(relative, expanded)
	} else {
		let display = worktree_root.join(&expanded);
		(expanded, display)
	};
	let metadata_worktree = worktree.try_clone()?;
	match crate::read_file_at(worktree, &relative, &display).await {
		Ok(Some(bytes)) => Ok(Some(bytes)),
		Ok(None) => Ok(None),
		Err(error) => {
			let has_kind = |kind| {
				error.chain().any(|cause| {
					cause
						.downcast_ref::<std::io::Error>()
						.is_some_and(|error| error.kind() == kind)
				})
			};
			if has_kind(std::io::ErrorKind::NotFound) {
				return Ok(None);
			}
			if has_kind(std::io::ErrorKind::PermissionDenied) {
				match metadata_worktree.metadata(&relative) {
					Ok(metadata) if !metadata.is_dir() => return Ok(None),
					Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
						return Ok(None);
					}
					Ok(_) | Err(_) => {}
				}
			}
			Err(error.context(format!(
				"cannot use {} as an exclude file",
				display.display()
			)))
		}
	}
}

/// Reject a `core.excludesFile` that is present but *valueless* (`[core]\n\texcludesFile`), which
/// git aborts with "missing value for 'core.excludesfile'": a distinct case from a genuinely absent
/// key, which selects the XDG default. Every occurrence is checked, even one a later value shadows,
/// as git does. For a caller that checks it without reading the file (`add -f`).
pub fn validate_excludes_file_setting(config: &GitConfig) -> Result<()> {
	if config
		.get_all_raw("core", None, "excludesfile")
		.iter()
		.any(Option::is_none)
	{
		anyhow::bail!("missing value for 'core.excludesfile'");
	}
	Ok(())
}

/// Translate an absolute spelling beneath `worktree_root` into a capability-relative path.
///
/// Repository discovery and a configured value can use different native aliases for the same
/// directory (notably `/private/var` and `/var` on macOS). After deinit displaces the checkout the
/// file itself is absent at the public name, so resolve the nearest existing ancestor and append the
/// unresolved suffix rather than requiring the final file to remain ambiently reachable.
pub(crate) async fn absolute_path_in_worktree(
	path: &Path,
	worktree_root: &Path,
) -> Option<PathBuf> {
	if let Ok(relative) = path.strip_prefix(worktree_root) {
		return Some(relative.to_owned());
	}
	let canonical_root = tokio::fs::canonicalize(worktree_root).await.ok()?;
	for ancestor in path.ancestors() {
		let Ok(canonical_ancestor) = tokio::fs::canonicalize(ancestor).await else {
			continue;
		};
		let Ok(base) = canonical_ancestor.strip_prefix(&canonical_root) else {
			continue;
		};
		let suffix = path.strip_prefix(ancestor).ok()?;
		return Some(base.join(suffix));
	}
	None
}

/// The content of git's standard excludes file, or `None`. An explicitly empty `core.excludesFile`
/// disables it (git's way); a set value is used (with `~` expansion, and a relative path resolved
/// against the worktree `root`); when unset, git's default `$XDG_CONFIG_HOME/git/ignore` (falling
/// back to `~/.config/git/ignore`). A missing file contributes no patterns, but a configured path
/// that is a folder is an error, as it is for git; a regular file that can't be read is a warning
/// git continues past, so it adds no patterns. The reads are async (offloaded) so a slow or
/// network-mounted excludes file never blocks the runtime.
async fn read_excludes_file(configured: Option<&str>, root: &Path) -> Result<Option<Vec<u8>>> {
	let path = match configured {
		Some("") => return Ok(None),
		Some(value) => {
			let expanded = expand_tilde(value)?;
			// A bare `~` under an empty `HOME` expands to the empty path: git treats that as no excludes
			// file and continues (probed vs git 2.55: `core.excludesFile=~` with `HOME=` exits 0, adds no
			// patterns), rather than resolving it against the worktree root and rejecting the root
			// directory. (`~/` still expands to the absolute `/`, which git, and the folder check below,
			// reject.)
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
		Ok(bytes) => Ok(Some(bytes)),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
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

/// Expand a leading `~` / `~/…` against `$HOME` (git's excludes-file tilde handling).
fn expand_tilde(path: &str) -> Result<PathBuf> {
	expand_tilde_in("core.excludesFile", path)
}

/// [`expand_tilde`] for the setting `key`, which an error names. `~user` is not supported and
/// yields an error; a path without a leading `~` is returned as-is.
pub(crate) fn expand_tilde_in(key: &str, path: &str) -> Result<PathBuf> {
	let Some(rest) = path.strip_prefix('~') else {
		return Ok(PathBuf::from(path));
	};
	if !rest.is_empty() && !rest.starts_with('/') {
		anyhow::bail!("unsupported `~user` in {key}: {path}");
	}
	// Only an *unset* `HOME` is fatal. `HOME` and the remainder are joined as strings, not as paths,
	// keeping the leading separator: an empty `HOME` expands `~/foo` to the absolute `/foo` (and a
	// bare `~` to the empty path, which the caller then treats as no file, as git does), rather than a
	// repository-relative path that could hide files. (Unlike the XDG default below, where an empty
	// `HOME` is treated as unset.)
	let mut expanded = crate::environment::var_os("HOME")
		.ok_or_else(|| anyhow::anyhow!("cannot expand `~`: $HOME is not set"))?;
	expanded.push(rest);
	Ok(PathBuf::from(expanded))
}

/// git's XDG base for user config: `$XDG_CONFIG_HOME` when set and non-empty, else `~/.config`. A
/// *relative* `$XDG_CONFIG_HOME` is used as-is (resolved against the process directory), a
/// documented best-effort gap versus git, which resolves it against the repository. An **empty**
/// `HOME` (as sanitized or container environments often set) is treated as unset, so the default
/// path is not silently resolved relative to the process directory.
pub(crate) fn xdg_config_home() -> Option<PathBuf> {
	if let Some(xdg) = crate::environment::var_os("XDG_CONFIG_HOME")
		&& !xdg.is_empty()
	{
		return Some(PathBuf::from(xdg));
	}
	crate::environment::var_os("HOME")
		.filter(|home| !home.is_empty())
		.map(|home| PathBuf::from(home).join(".config"))
}

#[cfg(test)]
mod tests {
	use super::read_excludes_file;

	#[tokio::test]
	async fn reads_exclude_patterns_as_exact_bytes() {
		let root = tempfile::tempdir().unwrap();
		std::fs::write(root.path().join("exclude"), b"raw-\xff\n").unwrap();

		let content = read_excludes_file(Some("exclude"), root.path())
			.await
			.unwrap();
		assert_eq!(content.as_deref(), Some(b"raw-\xff\n".as_slice()));
	}
}
