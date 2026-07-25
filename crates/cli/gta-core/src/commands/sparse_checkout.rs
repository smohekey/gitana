//! `gta sparse-checkout` — restrict the working tree to a subset of tracked paths (git's
//! sparse-checkout). The omitted paths keep their index entry (skip-worktree bit) and full history;
//! only their working-tree files are removed. The pattern model, config, and apply engine live in
//! [`gitana_worktree`]; this command parses the sub-action and drives that surface.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use gitana_object::HashAlgorithm;
use gitana_worktree::{SparseReapply, SparseSet, WorkTree};

use crate::Backend;
use crate::dispatch::{self, WorkTreeCommand};

/// A `gta sparse-checkout` operation.
pub enum Action {
	/// Enable sparse-checkout with the default set — cone: root files only; `--no-cone`: everything
	/// (`/*`), the neutral starting point a subsequent edit narrows. git still accepts `init`, though it
	/// steers users to `set`.
	Init { no_cone: bool },
	/// Replace the sparse-checkout set with `patterns` — cone directories, or (`--no-cone`)
	/// gitignore-style patterns — and apply it.
	Set {
		patterns: Vec<String>,
		no_cone: bool,
	},
	/// Extend the current sparse-checkout set with `patterns`, keeping the configured mode.
	Add { patterns: Vec<String> },
	/// Print the current sparse-checkout set (cone directories, or non-cone pattern lines).
	List,
	/// Disable sparse-checkout, materialising the whole working tree.
	Disable,
	/// Re-apply the current sparse-checkout patterns to the working tree (after a manual edit of
	/// `.git/info/sparse-checkout`, or to re-omit a path that was written back).
	Reapply,
}

/// Manage the working tree's sparse-checkout.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	dispatch::on_worktree(cwd, SparseCheckout { action }).await
}

struct SparseCheckout {
	action: Action,
}

impl WorkTreeCommand for SparseCheckout {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
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
				report_left(worktree.apply_sparse_set(&set).await?);
			}
			Action::Set { patterns, no_cone } => {
				let set = if no_cone {
					reject_noncone_subdir(&prefix)?;
					// An empty non-cone `set` is git's non-cone default (root files only), not an empty file
					// (which would omit even the root files).
					if patterns.is_empty() {
						noncone_default()
					} else {
						SparseSet::NonCone(patterns)
					}
				} else {
					let dirs = cone_dirs(&prefix, patterns)?;
					reject_tracked_files(&worktree, &dirs).await?;
					SparseSet::Cone(dirs)
				};
				report_left(worktree.apply_sparse_set(&set).await?);
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
						lines.extend(patterns);
						SparseSet::NonCone(lines)
					}
				};
				report_left(worktree.apply_sparse_set(&merged).await?);
			}
			Action::List => match worktree.current_sparse_set().await? {
				Some(set) => {
					for entry in set.entries() {
						println!("{entry}");
					}
				}
				None => bail!("this worktree is not sparse"),
			},
			Action::Disable => report_left(worktree.disable_sparse().await?),
			Action::Reapply => report_left(worktree.reapply_sparse().await?),
		}
		Ok(())
	}
}

/// git's non-cone default set — `/*` then `!/*/`, i.e. everything at the root with no directories, so
/// only root files are materialised (the same shape as the default cone set).
fn noncone_default() -> SparseSet {
	SparseSet::NonCone(vec!["/*".to_owned(), "!/*/".to_owned()])
}

/// git refuses a non-cone `set`/`add` run from a subdirectory: non-cone patterns are always evaluated
/// from the work-tree root, so a subdirectory invocation would be ambiguous (probed against git 2.50.1
/// — "please run from the toplevel directory in non-cone mode"). Cone mode, by contrast, resolves its
/// directory arguments against the prefix, so this restriction is non-cone only.
fn reject_noncone_subdir(prefix: &str) -> Result<()> {
	if !prefix.is_empty() {
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
	dirs: &[String],
) -> Result<()> {
	let index = worktree.load_index().await?;
	for dir in dirs {
		// The root ("") is always a directory; every other arg must not be an exact tracked-file path.
		if !dir.is_empty()
			&& index
				.entries
				.iter()
				.any(|entry| entry.stage == 0 && entry.path == *dir)
		{
			bail!("'{dir}' is a tracked file, not a directory");
		}
	}
	Ok(())
}

/// Resolve cone directory arguments against the invocation prefix — git interprets `set <dir>`
/// relative to the current directory, resolving `.`/`..` components (so `-C a/b set .` means the
/// recursive directory `a/b`, and `../x` climbs out of the prefix). A leading `/` is root-relative
/// (the prefix is ignored). A path that climbs above the work-tree root is rejected.
fn cone_dirs(prefix: &str, dirs: Vec<String>) -> Result<Vec<String>> {
	dirs
		.into_iter()
		.map(|dir| resolve_cone_dir(prefix, &dir))
		.collect()
}

/// Resolve one cone directory argument against `prefix`, collapsing `.`/`..` and rejecting an escape
/// above the root. Returns the work-tree-root-relative directory (empty string for the root).
fn resolve_cone_dir(prefix: &str, dir: &str) -> Result<String> {
	// Cone directories are literal paths, not globs: git rejects one containing pattern metacharacters
	// (without `--skip-checks`), because a stray `*`/`?`/`[` would silently disable cone matching and pull
	// in sibling directories. Reject them rather than render an invalid cone pattern.
	if dir.contains(['*', '?', '[', ']', '\\']) {
		bail!("'{dir}' contains a pattern character; cone directories must be literal paths");
	}
	// git rejects a leading slash in cone mode: a cone argument is a directory resolved against the
	// invocation prefix, not a root-anchored pattern (probed against git 2.50.1 — `set /x` fails with
	// "specify directories rather than patterns (no leading slash)").
	if dir.starts_with('/') {
		bail!("'{dir}': specify directories rather than patterns (no leading slash) in cone mode");
	}
	let mut components: Vec<&str> = Vec::new();
	for segment in prefix.split('/').chain(dir.split('/')) {
		match segment {
			"" | "." => {}
			".." => {
				if components.pop().is_none() {
					bail!("'{dir}' is outside the repository");
				}
			}
			segment => components.push(segment),
		}
	}
	Ok(components.join("/"))
}

/// Warn — as git does — about paths the reapply could not fully apply: one left in the working tree
/// because it had local modifications the reapply would otherwise have removed, and one that could not
/// be materialised because an untracked file occupies an ancestor slot. The user resolves those and
/// re-runs `reapply`.
fn report_left(outcome: SparseReapply) {
	for path in outcome.left_dirty {
		eprintln!("warning: '{path}' is not up to date and was left despite sparse patterns");
	}
	for path in outcome.not_updated {
		eprintln!("warning: '{path}' was already present and thus not updated despite sparse patterns");
	}
}

#[cfg(test)]
mod tests {
	use super::resolve_cone_dir;

	#[test]
	fn resolves_cone_dir_against_the_prefix() {
		// From the root, an argument is used as-is (normalised).
		assert_eq!(resolve_cone_dir("", "a/b").unwrap(), "a/b");
		// From a subdirectory, `.` is the recursive prefix directory, and a relative arg scopes under it.
		assert_eq!(resolve_cone_dir("a/b", ".").unwrap(), "a/b");
		assert_eq!(resolve_cone_dir("a/b", "c").unwrap(), "a/b/c");
		// `..` climbs out of the prefix.
		assert_eq!(resolve_cone_dir("a/b", "../x").unwrap(), "a/x");
		assert_eq!(resolve_cone_dir("a/b", "../../x").unwrap(), "x");
		// Climbing above the root is rejected.
		assert!(resolve_cone_dir("a/b", "../../../x").is_err());
		assert!(resolve_cone_dir("", "..").is_err());
	}

	#[test]
	fn rejects_a_leading_slash_in_cone_mode() {
		// git rejects a leading slash in cone `set`/`add` ("no leading slash") — it is not root-relative.
		assert!(resolve_cone_dir("", "/a").is_err());
		assert!(resolve_cone_dir("a/b", "/x").is_err());
	}
}
