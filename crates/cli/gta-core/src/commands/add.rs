use std::collections::HashSet;
use std::path::Path;

use crate::{Backend, ResultPathMode};
use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_path::{GitPath, GitPathspec};
use gitana_worktree::{LineEndingChange, WorkTree, WorktreeError};

use crate::dispatch::{self, WorkTreeCommand};
use crate::error::AddAdvisory;

/// Stage the given pathspecs (files, directories, or `.`), interpreted relative to `cwd`. `force`
/// (git's `-f`/`--force`) stages explicitly-named ignored paths that would otherwise be refused.
pub async fn run(
	cwd: &Path,
	pathspecs: &[GitPathspec],
	force: bool,
	result_path_mode: ResultPathMode,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Add {
			cwd: cwd.to_owned(),
			pathspecs,
			force,
			result_path_mode,
		},
	)
	.await
}

struct Add<'a> {
	cwd: std::path::PathBuf,
	pathspecs: &'a [GitPathspec],
	force: bool,
	result_path_mode: ResultPathMode,
}

impl WorkTreeCommand for Add<'_> {
	/// The directory the command runs in once the repository is held: the work tree's top when run
	/// from where `.git` is while `core.worktree` puts the work tree elsewhere, as git changes into it,
	/// so a relative excludes file is found there.
	fn set_command_directory(&mut self, path: std::path::PathBuf, _directory: cap_std::fs::Dir) {
		self.cwd = path;
	}

	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		// git reads (and validates) advice.updateSparsePath / advice.addIgnoredFile during `add` setup —
		// before touching the index — so a malformed boolean fails the command before anything is staged, on
		// every add (probed vs git 2.50.1). Read them up front for that fail-before-staging parity, and reuse
		// them to render the advisory: `false` suppresses that block's `hint:` lines (git's default shows
		// them).
		let config = worktree.repository().effective_config().await?;
		let show_sparse_hints = config
			.get_bool("advice", None, "updateSparsePath")?
			.unwrap_or(true);
		let show_ignored_hints = config
			.get_bool("advice", None, "addIgnoredFile")?
			.unwrap_or(true);
		// git's global excludes file (`core.excludesFile`) content, resolved here as it lives outside the
		// worktree. `-f` stages ignored paths regardless, so skip the *read* then — matching git, which does
		// not consult the excludes file when forcing — but still validate the setting: git aborts `add -f`
		// on a valueless `core.excludesFile` before staging (probed vs git 2.55).
		let excludes_file = if self.force {
			crate::excludes::validate_excludes_file_setting(&config)?;
			None
		} else {
			crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?
		};
		// The paths the index has before staging: git warns of their line endings before its ignored-path
		// advisory, and of new files' after it.
		let tracked: HashSet<GitPath> = worktree
			.load_index()
			.await?
			.entries
			.into_iter()
			.map(|entry| entry.path)
			.collect();
		match worktree
			.add_pathspecs(
				self.pathspecs,
				&prefix,
				self.force,
				excludes_file.as_deref(),
			)
			.await
		{
			Ok(outcome) => {
				report_line_endings(&outcome.line_ending_warnings, self.result_path_mode);
				Ok(())
			}
			// git stages everything it can, then exits non-zero rendering the sparse block (for out-of-cone
			// pathspecs) and/or the ignored block (for ignored pathspecs). The staged work is already saved,
			// and its line-ending warnings go where git prints them while staging.
			Err(WorktreeError::PathspecAdvisory {
				sparse,
				ignored,
				line_ending_warnings: warnings,
			}) => {
				let (tracked_warnings, new_warnings): (Vec<_>, Vec<_>) = warnings
					.into_iter()
					.partition(|(path, _)| tracked.contains(path));
				let sparse = sparse
					.iter()
					.map(|path| crate::git_path::render_result_pathspec(path, self.result_path_mode))
					.collect();
				let ignored = ignored
					.iter()
					.map(|path| crate::git_path::render_result_path(path, self.result_path_mode))
					.collect();
				Err(
					AddAdvisory::new(sparse, ignored, show_sparse_hints, show_ignored_hints)
						.with_line_ending_warnings(
							line_ending_warnings(&tracked_warnings, self.result_path_mode),
							line_ending_warnings(&new_warnings, self.result_path_mode),
						)
						.into(),
				)
			}
			Err(other) => Err(other.into()),
		}
	}
}

/// Warn, as git's `core.safecrlf=warn` (its default) does, of each staged file whose line endings
/// the next checkout will change.
fn report_line_endings(warnings: &[(GitPath, LineEndingChange)], result_path_mode: ResultPathMode) {
	for warning in line_ending_warnings(warnings, result_path_mode) {
		eprintln!("{warning}");
	}
}

fn line_ending_warnings(
	warnings: &[(GitPath, LineEndingChange)],
	result_path_mode: ResultPathMode,
) -> Vec<String> {
	warnings
		.iter()
		.map(|(path, change)| {
			let path = crate::git_path::render_result_path(path, result_path_mode);
			let (from, to) = match change {
				LineEndingChange::CrlfToLf => ("CRLF", "LF"),
				LineEndingChange::LfToCrlf => ("LF", "CRLF"),
			};
			format!(
				"warning: in the working copy of '{path}', {from} will be replaced by {to} the next time Git touches it"
			)
		})
		.collect()
}
