use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::{LsFilesConfig, LsFilesOptions, WorkTree, WorktreeError};

use crate::dispatch::{self, WorkTreeCommand};

/// List the paths tracked in the index (and, with the selection options, untracked / modified /
/// deleted working-tree paths), filtered by `pathspecs` and rendered git's way.
pub async fn run(cwd: &Path, pathspecs: &[String], opts: LsFilesOptions) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		LsFiles {
			cwd: cwd.to_owned(),
			pathspecs,
			opts,
		},
	)
	.await
}

struct LsFiles<'a> {
	cwd: PathBuf,
	pathspecs: &'a [String],
	opts: LsFilesOptions,
}

impl WorkTreeCommand for LsFiles<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		// Rendering and the modified/excludes checks resolve config across git's full stack; read it once.
		// These are git's startup booleans: it validates *every* occurrence (even a shadowed
		// lower-precedence one) and aborts on a malformed value, so use `get_bool_validated`.
		let config = worktree.repository().effective_config().await?;
		let quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let file_mode = config
			.get_bool_validated("core", None, "filemode")?
			.unwrap_or(true);
		let ignore_case = config
			.get_bool_validated("core", None, "ignorecase")?
			.unwrap_or(false);
		let symlinks = config
			.get_bool_validated("core", None, "symlinks")?
			.unwrap_or(true);
		// The standard excludes file lives outside the worktree (git's `core.excludesFile`, else the XDG
		// default), so resolve and read it here rather than in the sandboxed worktree crate. Only needed
		// for `-o --exclude-standard`.
		let excludes_file = if self.opts.others && self.opts.exclude_standard {
			let root = worktree_root(&self.cwd, &prefix);
			read_excludes_file(config.get_string("core", None, "excludesfile"), &root)?
		} else {
			None
		};

		let specs: Vec<&str> = self.pathspecs.iter().map(String::as_str).collect();
		let ls_config = LsFilesConfig {
			quote_path,
			file_mode,
			ignore_case,
			symlinks,
			excludes_file: excludes_file.as_deref(),
		};
		let output = worktree
			.ls_files(&specs, &prefix, &self.opts, &ls_config)
			.await?;
		// Written as bytes: under `-z` the output carries embedded NUL separators. git prints the matched
		// entries and *then* fails on an unmatched pathspec, so write before signalling the error.
		let mut stdout = std::io::stdout();
		stdout.write_all(output.text.as_bytes())?;
		stdout.flush()?;
		if let Some(spec) = output.unmatched {
			return Err(WorktreeError::PathspecMatch(spec).into());
		}
		Ok(())
	}
}

/// The working-tree root: the invocation directory `cwd` with the discovered `prefix` (its
/// `/`-joined path below the root) stripped. git resolves a relative `core.excludesFile` against this
/// root, not the current subdirectory. `cwd` is canonicalised first so a symlinked `-C` argument
/// still yields the real root the `prefix` was measured against.
fn worktree_root(cwd: &Path, prefix: &str) -> PathBuf {
	let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
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
/// configured path that is a directory or is unreadable — is an error, as it is for git.
fn read_excludes_file(configured: Option<&str>, root: &Path) -> Result<Option<String>> {
	let path = match configured {
		Some("") => return Ok(None), // explicitly disabled
		Some(value) => {
			let expanded = expand_tilde(value)?;
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
	match std::fs::read(&path) {
		Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
		// A missing file is fine (a configured-but-absent path, or no XDG default, just adds no patterns).
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		// git is fatal ("cannot use … as an exclude file") only when the path is a *directory*; a regular
		// file that cannot be read (permission-denied) is a warning it continues past — so it just adds no
		// patterns here.
		Err(error) => {
			if path.is_dir() {
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
	let home = std::env::var_os("HOME")
		.ok_or_else(|| anyhow::anyhow!("cannot expand `~`: $HOME is not set"))?;
	Ok(PathBuf::from(home).join(rest.strip_prefix('/').unwrap_or(rest)))
}

/// git's XDG base for user config: `$XDG_CONFIG_HOME` when set and non-empty, else `~/.config`. A
/// *relative* `$XDG_CONFIG_HOME` is used as-is (resolved against the process directory), a documented
/// best-effort gap versus git, which resolves it against the repository — a pathological configuration.
fn xdg_config_home() -> Option<PathBuf> {
	if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
		&& !xdg.is_empty()
	{
		return Some(PathBuf::from(xdg));
	}
	std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}
