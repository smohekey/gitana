//! Config-include resolution over the component's `FileStore` capability.

use std::path::{Component, Path, PathBuf};

use gitana_config::{ConfigError, IncludeResolver};
use gitana_file_store::FileStore;

/// An [`IncludeResolver`] over the component's [`FileStore`] capability. Include targets are read
/// relative to the store root — the common directory the `config` lives in — because the component holds
/// a path-less `wasi:filesystem` descriptor, not an ambient path.
///
/// Resolution follows git's *filesystem* semantics as far as the capability allows:
/// - An absolute path escapes the capability → skipped (reported absent), as git skips an unreachable
///   include.
/// - A `..` component matches git's traversal: the directory it climbs out of must exist, so each `..`
///   is admitted only when the accumulated prefix is an existing directory (`is_dir`); a `..` through a
///   missing or non-directory prefix — or one that would climb above the root — skips the include, as
///   git skips the resulting `ENOENT`/`ENOTDIR`. (git resolves a symlinked prefix through the link,
///   which the path-less store cannot — no `realpath` — so a `..` *after a symlink* is the one residual
///   divergence; realistic include paths do not use it.)
/// - A directory target aborts (git fatals `unable to access … Is a directory`); any other read failure
///   (absent, `ENOTDIR`, inaccessible) skips, as git warns and continues.
///
/// A `~`/`~user` include never reaches the resolver: with no `$HOME` the engine fails it up front
/// (`IncludeTildeNoHome`), which is what git itself does when `HOME` is unset — so a repository with a
/// `~` include is un-openable here, as under git with no home. The store has no symlink notion, so the
/// reported canonical path is the requested path unchanged (the lexical/real directory split collapses).
pub(crate) struct FileStoreIncludeResolver<'a, F> {
	store: &'a F,
}

impl<'a, F> FileStoreIncludeResolver<'a, F> {
	pub(crate) fn new(store: &'a F) -> Self {
		Self { store }
	}
}

impl<F: FileStore> IncludeResolver for FileStoreIncludeResolver<'_, F> {
	async fn read(&self, path: &Path) -> Result<Option<(String, PathBuf)>, ConfigError> {
		let relative = match self.resolve(path).await? {
			Resolved::Path(relative) => relative,
			// Outside the capability, or a `..` git would not traverse — unreadable, so skipped.
			Resolved::Skip => return Ok(None),
			// The include resolves to the (config) directory itself (`path =`, `.`, `sub/..`); git aborts
			// on a directory target, so this does too, before any read.
			Resolved::Directory => return Err(directory_error(path)),
		};
		// git aborts on a directory include target; every other access failure it warns about and skips.
		match self.store.is_dir(&relative).await {
			Ok(true) => Err(directory_error(path)),
			_ => match self.store.read_path(&relative).await {
				Ok(bytes) => {
					let text = String::from_utf8(bytes)
						.map_err(|_| ConfigError::Parse(format!("include {} is not UTF-8", path.display())))?;
					// No symlink resolution in the store: the canonical path is the requested path.
					Ok(Some((text, path.to_path_buf())))
				}
				// Absent / `ENOTDIR` / inaccessible — git warns and continues, so skip.
				Err(_) => Ok(None),
			},
		}
	}
}

impl<F: FileStore> FileStoreIncludeResolver<'_, F> {
	/// Classify a resolved include path against the capability, following git's filesystem `..` traversal
	/// (probed by `is_dir`): a real store-relative path, a skip (unreachable — absolute, non-UTF-8, or a
	/// `..` git would not traverse), or the root directory itself (an empty result — `path =`, `.`,
	/// `sub/..` — which git treats as a directory target). `is_dir` failures resolve to a skip, matching
	/// git skipping `ENOENT`/`ENOTDIR`.
	async fn resolve(&self, path: &Path) -> Result<Resolved, ConfigError> {
		if path.is_absolute() {
			return Ok(Resolved::Skip);
		}
		let mut parts: Vec<&str> = Vec::new();
		for component in path.components() {
			match component {
				Component::Normal(part) => match part.to_str() {
					Some(part) => parts.push(part),
					// Non-UTF-8 — the store addresses files by UTF-8 string, so unreachable.
					None => return Ok(Resolved::Skip),
				},
				// A `./` segment is a no-op within the root.
				Component::CurDir => {}
				Component::ParentDir => {
					// Climbing above the root escapes the capability.
					if parts.is_empty() {
						return Ok(Resolved::Skip);
					}
					// git's `..` requires the directory it climbs out of to exist; a missing or non-directory
					// prefix (`is_dir` not `Ok(true)`) is `ENOENT`/`ENOTDIR`, which git skips.
					let prefix = parts.join("/");
					if !matches!(self.store.is_dir(&prefix).await, Ok(true)) {
						return Ok(Resolved::Skip);
					}
					parts.pop();
				}
				// An absolute root or a Windows prefix escapes the capability.
				Component::RootDir | Component::Prefix(_) => return Ok(Resolved::Skip),
			}
		}
		// No components left means the include names the (config) directory itself — a directory target.
		Ok(if parts.is_empty() {
			Resolved::Directory
		} else {
			Resolved::Path(parts.join("/"))
		})
	}
}

/// The outcome of resolving an include path against the [`FileStore`] capability
/// ([`FileStoreIncludeResolver::resolve`]).
enum Resolved {
	/// A store-relative path to read.
	Path(String),
	/// Unreachable within the capability — skip the include (git skips an unreachable/absent target).
	Skip,
	/// The include resolves to the root directory itself — a directory target, which git aborts on.
	Directory,
}

/// git's fatal for a directory include target (`unable to access … Is a directory`).
fn directory_error(path: &Path) -> ConfigError {
	ConfigError::Parse(format!(
		"unable to access include {}: is a directory",
		path.display()
	))
}
