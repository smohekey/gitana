//! `gta sparse-checkout` — restrict the working tree to a subset of tracked paths (git's
//! sparse-checkout). The omitted paths keep their index entry (skip-worktree bit) and full history;
//! only their working-tree files are removed. The pattern model, config, and apply engine live in
//! [`gitana_worktree`]; this command parses the sub-action and drives that surface.

use std::borrow::Cow;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use gitana_object::HashAlgorithm;
use gitana_path::{GitPath, GitPathspec};
use gitana_worktree::{SparseReapply, SparseSet, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};
use crate::{Backend, ResultPathMode};

use super::SparseCheckoutError;

/// A `gta sparse-checkout` operation.
pub enum Action {
	/// Enable sparse-checkout with the default set — cone: root files only; `--no-cone`: everything
	/// (`/*`), the neutral starting point a subsequent edit narrows. git still accepts `init`, though it
	/// steers users to `set`.
	Init { no_cone: bool },
	/// Replace the sparse-checkout set with `patterns` — cone directories, or (`--no-cone`)
	/// gitignore-style patterns — and apply it.
	Set {
		patterns: Vec<GitPathspec>,
		no_cone: bool,
	},
	/// Extend the current sparse-checkout set with `patterns`, keeping the configured mode.
	Add { patterns: Vec<GitPathspec> },
	/// Print the current sparse-checkout set (cone directories, or non-cone pattern lines).
	List,
	/// Disable sparse-checkout, materialising the whole working tree.
	Disable,
	/// Re-apply the current sparse-checkout patterns to the working tree (after a manual edit of
	/// `.git/info/sparse-checkout`, or to re-omit a path that was written back).
	Reapply,
}

/// Manage the working tree's sparse-checkout.
pub async fn run(cwd: &Path, action: Action, result_path_mode: ResultPathMode) -> Result<()> {
	let writes_shared_config = matches!(
		&action,
		Action::Init { .. } | Action::Set { .. } | Action::Add { .. } | Action::Disable
	);
	let command = SparseCheckout {
		action,
		result_path_mode,
	};
	if writes_shared_config {
		dispatch::on_worktree_config_mutation(cwd, command).await
	} else {
		dispatch::on_worktree_config_read(cwd, command).await
	}
}

struct SparseCheckout {
	action: Action,
	result_path_mode: ResultPathMode,
}

impl WorkTreeCommand for SparseCheckout {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: gitana_path::GitPath,
	) -> Result<()> {
		match self.action {
			Action::Init { no_cone } => {
				// git's `init` reapplies the existing set when sparse-checkout is already configured, rather
				// than replacing it — so `init` after `set a` keeps `a`. Only a first-time `init` uses the
				// default set (cone: root files only; non-cone: everything, `/*`).
				let set = match worktree.current_sparse_set().await? {
					// Reuse the configured set only when its mode matches the requested one. git keeps the
					// pattern file across a mode switch (flipping only the config bit); gitana resets to the
					// mode's default rather than reinterpret cone directories as non-cone patterns — a
					// documented minor deviation for the reinit-mode-switch corner.
					Some(existing) if existing.is_cone() != no_cone => existing,
					_ if no_cone => noncone_default(),
					_ => SparseSet::Cone(Vec::new()),
				};
				report_left(
					worktree.apply_sparse_set(&set).await?,
					self.result_path_mode,
				);
			}
			Action::Set { patterns, no_cone } => {
				let set = if no_cone {
					reject_noncone_subdir(&prefix)?;
					// An empty non-cone `set` is git's non-cone default (root files only), not an empty file
					// (which would omit even the root files).
					if patterns.is_empty() {
						noncone_default()
					} else {
						SparseSet::NonCone(pattern_bytes(patterns)?)
					}
				} else {
					let dirs = cone_dirs(&prefix, patterns)?;
					reject_tracked_files(&worktree, &dirs).await?;
					SparseSet::Cone(dirs)
				};
				report_left(
					worktree.apply_sparse_set(&set).await?,
					self.result_path_mode,
				);
			}
			Action::Add { patterns } => {
				let current = worktree
					.current_sparse_set()
					.await?
					.ok_or_else(|| anyhow!("run 'gta sparse-checkout init' or 'set' before 'add'"))?;
				let merged = match current {
					// `add` keeps the configured mode: cone appends directories, non-cone appends patterns.
					SparseSet::Cone(mut dirs) => {
						let new = cone_dirs(&prefix, patterns)?;
						reject_tracked_files(&worktree, &new).await?;
						dirs.extend(new);
						SparseSet::Cone(dirs)
					}
					SparseSet::NonCone(mut lines) => {
						reject_noncone_subdir(&prefix)?;
						lines.extend(pattern_bytes(patterns)?);
						SparseSet::NonCone(lines)
					}
				};
				report_left(
					worktree.apply_sparse_set(&merged).await?,
					self.result_path_mode,
				);
			}
			Action::List => match worktree.current_sparse_set().await? {
				Some(set) => {
					let mut stdout = std::io::stdout().lock();
					for entry in set.entries() {
						let entry = render_sparse_entry(entry, self.result_path_mode);
						stdout.write_all(&entry)?;
						stdout.write_all(b"\n")?;
					}
					stdout.flush()?;
				}
				None => bail!("this worktree is not sparse"),
			},
			Action::Disable => report_left(worktree.disable_sparse().await?, self.result_path_mode),
			Action::Reapply => report_left(worktree.reapply_sparse().await?, self.result_path_mode),
		}
		Ok(())
	}
}

/// git's non-cone default set — `/*` then `!/*/`, i.e. everything at the root with no directories, so
/// only root files are materialised (the same shape as the default cone set).
fn noncone_default() -> SparseSet {
	SparseSet::non_cone_utf8(["/*", "!/*/"])
}

/// git refuses a non-cone `set`/`add` run from a subdirectory: non-cone patterns are always evaluated
/// from the work-tree root, so a subdirectory invocation would be ambiguous (probed against git 2.50.1
/// — "please run from the toplevel directory in non-cone mode"). Cone mode, by contrast, resolves its
/// directory arguments against the prefix, so this restriction is non-cone only.
fn reject_noncone_subdir(prefix: &gitana_path::GitPath) -> Result<()> {
	if !prefix.is_root() {
		bail!("please run from the toplevel directory in non-cone mode");
	}
	Ok(())
}

/// git refuses a cone directory argument that names a tracked *file*: a cone set takes directories, and
/// a file argument would match nothing. git checks the index for an exact entry (probed against git
/// 2.50.1 — an untracked or nonexistent path is fine, only a tracked file errors); there is no
/// `--skip-checks` escape in gta.
async fn reject_tracked_files<H: HashAlgorithm>(
	worktree: &WorkTree<Backend, crate::WorkDir, H>,
	dirs: &[GitPath],
) -> Result<()> {
	let index = worktree.load_index().await?;
	for dir in dirs {
		// The root ("") is always a directory; every other arg must not be an exact tracked-file path.
		if !dir.is_root()
			&& index
				.entries
				.iter()
				.any(|entry| entry.stage == 0 && entry.path == *dir)
		{
			return Err(SparseCheckoutError::TrackedFile(dir.clone()).into());
		}
	}
	Ok(())
}

/// Resolve cone directory arguments against the invocation prefix — git interprets `set <dir>`
/// relative to the current directory, resolving `.`/`..` components (so `-C a/b set .` means the
/// recursive directory `a/b`, and `../x` climbs out of the prefix). A leading `/` or a path that
/// climbs above the work-tree root is rejected.
fn cone_dirs(prefix: &GitPath, dirs: Vec<GitPathspec>) -> Result<Vec<GitPath>> {
	dirs
		.into_iter()
		.map(|dir| resolve_cone_dir(prefix, &dir))
		.collect()
}

/// Resolve one cone directory argument against `prefix`, collapsing `.`/`..` and rejecting an escape
/// above the root. Returns the work-tree-root-relative directory (empty string for the root).
fn resolve_cone_dir(prefix: &GitPath, dir: impl AsRef<[u8]>) -> Result<GitPath> {
	let dir = GitPathspec::from_bytes(dir.as_ref().to_vec())?;
	let bytes = dir.as_bytes();
	// Cone directories are literal paths, not globs: git rejects one containing pattern metacharacters
	// (without `--skip-checks`), because a stray `*`/`?`/`[` would silently disable cone matching and pull
	// in sibling directories. Reject them rather than render an invalid cone pattern.
	if bytes
		.iter()
		.any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
	{
		return Err(SparseCheckoutError::PatternCharacter(dir).into());
	}
	// git rejects a leading slash in cone mode: a cone argument is a directory resolved against the
	// invocation prefix, not a root-anchored pattern (probed against git 2.50.1 — `set /x` fails with
	// "specify directories rather than patterns (no leading slash)").
	if bytes.starts_with(b"/") {
		return Err(SparseCheckoutError::LeadingSlash(dir).into());
	}
	let mut components: Vec<Vec<u8>> = prefix.components().map(<[u8]>::to_vec).collect();
	for segment in bytes.split(|byte| *byte == b'/') {
		match segment {
			b"" | b"." => {}
			b".." => {
				if components.pop().is_none() {
					return Err(SparseCheckoutError::OutsideRepository(dir).into());
				}
			}
			segment => components.push(segment.to_vec()),
		}
	}
	let mut bytes = Vec::new();
	for (index, component) in components.iter().enumerate() {
		if index > 0 {
			bytes.push(b'/');
		}
		bytes.extend_from_slice(component);
	}
	GitPath::from_bytes(bytes).map_err(Into::into)
}

fn pattern_bytes(patterns: Vec<GitPathspec>) -> Result<Vec<Vec<u8>>> {
	Ok(patterns.into_iter().map(GitPathspec::into_bytes).collect())
}

fn render_sparse_entry(entry: &[u8], result_path_mode: ResultPathMode) -> Cow<'_, [u8]> {
	match result_path_mode {
		ResultPathMode::Human => Cow::Borrowed(entry),
		ResultPathMode::Reversible => Cow::Owned(gitana_path::quote_bytes(entry).into_bytes()),
	}
}

/// Warn — as git does — about paths the reapply could not fully apply: one left in the working tree
/// because it had local modifications the reapply would otherwise have removed, and one that could not
/// be materialised because an untracked file occupies an ancestor slot. The user resolves those and
/// re-runs `reapply`.
fn report_left(outcome: SparseReapply, result_path_mode: ResultPathMode) {
	for warning in sparse_warnings(&outcome, result_path_mode) {
		eprintln!("{warning}");
	}
}

fn sparse_warnings(outcome: &SparseReapply, result_path_mode: ResultPathMode) -> Vec<String> {
	let left_dirty = outcome.left_dirty.iter().map(|path| {
		let path = crate::git_path::render_result_path(path, result_path_mode);
		format!("warning: '{path}' is not up to date and was left despite sparse patterns")
	});
	let not_updated = outcome.not_updated.iter().map(|path| {
		let path = crate::git_path::render_result_path(path, result_path_mode);
		format!("warning: '{path}' was already present and thus not updated despite sparse patterns")
	});
	left_dirty.chain(not_updated).collect()
}

#[cfg(test)]
mod tests {
	use gitana_path::GitPath;
	use gitana_worktree::SparseReapply;

	use crate::ResultPathMode;

	use super::{render_sparse_entry, resolve_cone_dir, sparse_warnings};

	fn path(value: &str) -> GitPath {
		GitPath::from_utf8(value).unwrap()
	}

	#[test]
	fn resolves_cone_dir_against_the_prefix() {
		// From the root, an argument is used as-is (normalised).
		assert_eq!(resolve_cone_dir(&path(""), "a/b").unwrap(), "a/b");
		// From a subdirectory, `.` is the recursive prefix directory, and a relative arg scopes under it.
		assert_eq!(resolve_cone_dir(&path("a/b"), ".").unwrap(), "a/b");
		assert_eq!(resolve_cone_dir(&path("a/b"), "c").unwrap(), "a/b/c");
		// `..` climbs out of the prefix.
		assert_eq!(resolve_cone_dir(&path("a/b"), "../x").unwrap(), "a/x");
		assert_eq!(resolve_cone_dir(&path("a/b"), "../../x").unwrap(), "x");
		// Climbing above the root is rejected.
		assert!(resolve_cone_dir(&path("a/b"), "../../../x").is_err());
		assert!(resolve_cone_dir(&path(""), "..").is_err());
	}

	#[test]
	fn rejects_a_leading_slash_in_cone_mode() {
		// git rejects a leading slash in cone `set`/`add` ("no leading slash") — it is not root-relative.
		assert!(resolve_cone_dir(&path(""), "/a").is_err());
		assert!(resolve_cone_dir(&path("a/b"), "/x").is_err());
	}

	#[test]
	fn preserves_a_raw_invocation_prefix() {
		let prefix = GitPath::from_bytes(b"raw-\xff/sub".to_vec()).unwrap();
		assert_eq!(
			resolve_cone_dir(&prefix, "../kept").unwrap().as_bytes(),
			b"raw-\xff/kept"
		);
	}

	#[test]
	fn sparse_list_entries_use_the_frontend_path_mode() {
		let raw = b"raw-\xff/**";
		let literal = b"\"raw-\\377/**\"";

		assert_eq!(
			render_sparse_entry(raw, ResultPathMode::Human).as_ref(),
			raw.as_slice()
		);
		assert_eq!(
			render_sparse_entry(raw, ResultPathMode::Reversible).as_ref(),
			b"\"raw-\\377/**\"".as_slice()
		);
		assert_ne!(
			render_sparse_entry(raw, ResultPathMode::Reversible),
			render_sparse_entry(literal, ResultPathMode::Reversible)
		);
	}

	#[test]
	fn sparse_warnings_use_the_frontend_path_mode() {
		let utf8 = SparseReapply {
			left_dirty: vec![path("café")],
			not_updated: vec![path("dir/file")],
		};
		assert_eq!(
			sparse_warnings(&utf8, ResultPathMode::Human),
			[
				"warning: 'café' is not up to date and was left despite sparse patterns",
				"warning: 'dir/file' was already present and thus not updated despite sparse patterns",
			]
		);

		let outcome = |path: GitPath| SparseReapply {
			left_dirty: vec![path.clone()],
			not_updated: vec![path],
		};
		let raw = outcome(GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap());
		let literal = outcome(GitPath::from_utf8("\"raw-\\377\"").unwrap());
		let raw_human = sparse_warnings(&raw, ResultPathMode::Human);
		let literal_human = sparse_warnings(&literal, ResultPathMode::Human);
		let raw_reversible = sparse_warnings(&raw, ResultPathMode::Reversible);
		let literal_reversible = sparse_warnings(&literal, ResultPathMode::Reversible);

		assert_eq!(raw_human, literal_human);
		assert_eq!(raw_reversible.len(), 2);
		assert_eq!(
			raw_reversible.len(),
			literal_reversible.len(),
			"both warning categories must be rendered"
		);
		for (raw, literal) in raw_reversible.iter().zip(&literal_reversible) {
			assert_ne!(raw, literal);
		}
	}
}
