use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Backend, PathQuoteMode};
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_path::GitPathspec;
use gitana_worktree::{LsFilesConfig, LsFilesOptions, WorkTree, WorktreeError};

use crate::dispatch::{self, WorkTreeCommand};

/// List the paths tracked in the index (and, with the selection options, untracked / modified /
/// deleted working-tree paths), filtered by `pathspecs` and rendered git's way.
pub async fn run(
	cwd: &Path,
	pathspecs: &[GitPathspec],
	opts: LsFilesOptions,
	quote_path: PathQuoteMode,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		LsFiles {
			cwd: cwd.to_owned(),
			pathspecs,
			opts,
			quote_path,
		},
	)
	.await
}

struct LsFiles<'a> {
	cwd: PathBuf,
	pathspecs: &'a [GitPathspec],
	opts: LsFilesOptions,
	quote_path: PathQuoteMode,
}

impl WorkTreeCommand for LsFiles<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		// Rendering and the modified/excludes checks resolve config across git's full stack; read it once.
		// These are git's startup booleans: it validates *every* occurrence (even a shadowed
		// lower-precedence one) and aborts on a malformed value, so use `get_bool_validated`.
		let config = worktree.repository().effective_config().await?;
		let configured_quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let quote_path = match self.quote_path {
			PathQuoteMode::Config => configured_quote_path,
			PathQuoteMode::Always => true,
		};
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
			crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?
		} else {
			None
		};

		let ls_config = LsFilesConfig {
			quote_path,
			file_mode,
			ignore_case,
			symlinks,
			excludes_file: excludes_file.as_deref(),
		};
		let output = worktree
			.ls_files_pathspecs(self.pathspecs, &prefix, &self.opts, &ls_config)
			.await?;
		// Written as bytes: under `-z` the output carries embedded NUL separators. git prints the matched
		// entries and *then* fails on an unmatched pathspec, so write before signalling the error.
		let mut stdout = std::io::stdout();
		stdout.write_all(&output.text)?;
		stdout.flush()?;
		if let Some(spec) = output.unmatched {
			return Err(WorktreeError::PathspecMatch(spec).into());
		}
		Ok(())
	}
}
