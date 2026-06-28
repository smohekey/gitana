//! Materialise a tree into the working directory and index.
//!
//! Writes regular files (with the exec bit), symlinks, and removes files absent
//! from the target tree; updates the index to match. Without `force` it refuses to
//! overwrite uncommitted local changes. Paths are validated against traversal,
//! `.git`, and symlinked ancestors (the git checkout CVE class).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gitana_file_store::FileStore;
use gitana_object::ObjectId;

use crate::fsmeta::{blob_of, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{IndexEntry, WorkTree, WorktreeError};

pub(crate) async fn run<F>(
	wt: &WorkTree<F>,
	tree: ObjectId,
	force: bool,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	let target = wt.repository().read_tree(tree).await?;
	let target_paths: HashMap<&str, (&str, ObjectId)> = target
		.iter()
		.map(|(path, mode, oid)| (path.as_str(), (mode.as_str(), *oid)))
		.collect();

	let mut index = wt.load_index()?;
	let current: HashMap<String, (String, ObjectId)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();

	if !force {
		let tracked: HashSet<&str> = current.keys().map(String::as_str).collect();
		for (path, (mode, oid)) in &target_paths {
			let differs = current
				.get(*path)
				.is_none_or(|(cm, co)| cm != mode || co != oid);
			if !differs {
				continue;
			}
			let full = wt.work_dir().join(path);
			match std::fs::symlink_metadata(&full) {
				// A directory occupies this file's slot (a directory->file change). Replacing it
				// would delete everything under it, so refuse if it holds a non-ignored untracked
				// file; its tracked contents are validated for cleanliness by the removal loop.
				Ok(meta) if meta.is_dir() && !meta.is_symlink() => {
					let mut stack = ignore_prefix(wt.work_dir(), path)?;
					if let Some(untracked) = first_untracked_under(wt.work_dir(), path, &tracked, &mut stack)?
					{
						return Err(WorktreeError::UntrackedOverwrite(untracked));
					}
				}
				_ => ensure_no_overwrite(wt, path, current.get(*path))?,
			}
			// A file->directory change removes a file occupying an ancestor slot; refuse if that
			// file is an untracked, non-ignored file (a tracked ancestor is validated by the
			// removal loop, an ignored one is expendable).
			if let Some(untracked) = untracked_file_ancestor(wt.work_dir(), path, &tracked)? {
				return Err(WorktreeError::UntrackedOverwrite(untracked));
			}
		}
		for path in current.keys() {
			if !target_paths.contains_key(path.as_str()) {
				ensure_no_overwrite(wt, path, current.get(path))?;
			}
		}
	}

	for (path, mode, oid) in &target {
		write_entry(wt, path, mode, *oid, &mut index).await?;
	}
	for path in current.keys() {
		if !target_paths.contains_key(path.as_str()) {
			remove_worktree_path(wt, path);
			index.remove(path);
		}
	}

	wt.save_index(&index)
}

/// Write `path`'s blob into the working tree and record it in the index. Combines a
/// working-tree write with the matching index upsert; used to materialise a whole tree.
pub(crate) async fn write_entry<F>(
	wt: &WorkTree<F>,
	path: &str,
	mode: &str,
	oid: ObjectId,
	index: &mut crate::Index,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	write_worktree_file(wt, path, mode, oid).await?;
	let meta = std::fs::symlink_metadata(wt.work_dir().join(path))?;
	index.upsert(IndexEntry {
		stat: stat_of(&meta),
		mode: u32::from_str_radix(mode, 8).unwrap_or(0o100644),
		oid,
		stage: 0,
		assume_valid: false,
		path: path.to_owned(),
	});
	Ok(())
}

/// Write `path`'s blob into the working tree only, without touching the index. Validates the
/// path against the checkout CVE class, creates parents, and replaces whatever occupies the
/// destination (a file, symlink, or directory).
pub(crate) async fn write_worktree_file<F>(
	wt: &WorkTree<F>,
	path: &str,
	mode: &str,
	oid: ObjectId,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	validate_path(path)?;
	let full = ensure_parents(wt.work_dir(), path)?;
	let content = wt.repository().read_blob(oid).await?;

	if mode == "120000" {
		clear_dest(&full)?;
		symlink(&String::from_utf8_lossy(&content), &full)?;
	} else {
		// Replace a directory (a directory->file type change) or a symlink at the destination;
		// a plain file is overwritten in place by the write below.
		match std::fs::symlink_metadata(&full) {
			Ok(meta) if meta.is_dir() && !meta.is_symlink() => std::fs::remove_dir_all(&full)?,
			Ok(meta) if meta.is_symlink() => std::fs::remove_file(&full)?,
			_ => {}
		}
		std::fs::write(&full, &content)?;
		set_mode(&full, mode);
	}
	Ok(())
}

/// Remove `path` from the working tree (ignoring an already-absent file) and prune any
/// directories left empty above it. Does not touch the index.
pub(crate) fn remove_worktree_path<F>(wt: &WorkTree<F>, path: &str)
where
	F: FileStore,
{
	let full = wt.work_dir().join(path);
	let _ = std::fs::remove_file(&full);
	remove_empty_parents(wt.work_dir(), path);
}

/// Like [`remove_worktree_path`], but reports a removal failure. An already-absent file is fine;
/// any other error (e.g. the path is now occupied by a directory) is returned so the caller can
/// refuse rather than silently leave the file in place.
pub(crate) fn remove_worktree_file<F>(wt: &WorkTree<F>, path: &str) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	let full = wt.work_dir().join(path);
	match std::fs::remove_file(&full) {
		Ok(()) => {}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}
	remove_empty_parents(wt.work_dir(), path);
	Ok(())
}

fn ensure_no_overwrite<F>(
	wt: &WorkTree<F>,
	path: &str,
	current: Option<&(String, ObjectId)>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	let full = wt.work_dir().join(path);
	let meta = match std::fs::symlink_metadata(&full) {
		Ok(meta) => meta,
		// Absent, or unreachable because a file occupies an ancestor directory (`ENOTDIR`):
		// either way there is nothing at `path` to overwrite.
		Err(error)
			if matches!(
				error.kind(),
				std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
			) =>
		{
			return Ok(());
		}
		Err(error) => return Err(error.into()),
	};
	match current {
		// Tracked: a conflict only if the working file is dirty vs the index.
		Some((mode, oid)) => match blob_of(&full, &meta)? {
			Some((woid, wmode)) if woid == *oid && format!("{wmode:o}") == *mode => Ok(()),
			_ => Err(WorktreeError::Conflict(path.to_owned())),
		},
		// Untracked file in the way of a checked-out path — refuse unless it is `.gitignore`d
		// (ignored files are expendable, as git overwrites them).
		None if path_ignored(wt.work_dir(), path)? => Ok(()),
		None => Err(WorktreeError::UntrackedOverwrite(path.to_owned())),
	}
}

/// Whether `path` (a file) is matched by the `.gitignore` rules from the work-tree root down to
/// its parent directory.
fn path_ignored(root: &Path, path: &str) -> Result<bool, WorktreeError> {
	let stack = ignore_prefix(root, path)?;
	Ok(ignore::is_ignored(path, false, &stack))
}

pub(crate) fn validate_path(path: &str) -> Result<(), WorktreeError> {
	for part in path.split('/') {
		if part.is_empty()
			|| part == "."
			|| part == ".."
			|| part.eq_ignore_ascii_case(".git")
			|| part.contains('\0')
		{
			return Err(WorktreeError::UnsafePath(path.to_owned()));
		}
	}
	Ok(())
}

/// Create the parent directories of `path` under `root`, refusing to traverse a
/// symlinked ancestor, and return the full path. A regular file occupying a directory slot is
/// replaced by the directory (a file->directory type change, as git checkout does); a symlink
/// is never traversed or removed here (the checkout CVE class).
fn ensure_parents(root: &Path, path: &str) -> Result<PathBuf, WorktreeError> {
	let mut full = root.to_path_buf();
	let parts: Vec<&str> = path.split('/').collect();
	for part in &parts[..parts.len().saturating_sub(1)] {
		full.push(part);
		match std::fs::symlink_metadata(&full) {
			Ok(meta) if meta.is_dir() && !meta.is_symlink() => {}
			Ok(meta) if meta.is_symlink() => return Err(WorktreeError::UnsafePath(path.to_owned())),
			Ok(_) => {
				std::fs::remove_file(&full)?;
				std::fs::create_dir(&full)?;
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				std::fs::create_dir(&full)?;
			}
			Err(error) => return Err(error.into()),
		}
	}
	full.push(parts.last().copied().unwrap_or_default());
	Ok(full)
}

/// The first non-ignored untracked path found anywhere under the working-tree directory
/// `dir_rel` — a file (or symlink) whose path is not a tracked index entry and is not matched
/// by `.gitignore`. Replacing the directory with a file would delete it, so a no-force checkout
/// must refuse. `.gitignore`d files (and whole ignored subtrees) are expendable, as in git, so
/// they don't block. `stack` is the ignore stack accumulated from the work-tree root down to
/// `dir_rel`'s parent; this descends `dir_rel`, pushing its own `.gitignore`.
fn first_untracked_under(
	root: &Path,
	dir_rel: &str,
	tracked: &HashSet<&str>,
	stack: &mut Vec<DirIgnore>,
) -> Result<Option<String>, WorktreeError> {
	// A wholly-ignored directory is expendable — git doesn't descend into it.
	if ignore::is_ignored(dir_rel, true, stack) {
		return Ok(None);
	}
	let pushed = push_gitignore(&root.join(dir_rel), dir_rel, stack)?;
	let mut found = None;
	for entry in std::fs::read_dir(root.join(dir_rel))? {
		let entry = entry?;
		let name = entry.file_name();
		let name = name.to_string_lossy();
		if name == ".git" {
			continue;
		}
		let rel = format!("{dir_rel}/{name}");
		let is_dir = entry.file_type()?.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue; // ignored content is expendable
		}
		if is_dir {
			if let Some(hit) = first_untracked_under(root, &rel, tracked, stack)? {
				found = Some(hit);
				break;
			}
		} else if !tracked.contains(rel.as_str()) {
			found = Some(rel);
			break;
		}
	}
	if pushed {
		stack.pop();
	}
	Ok(found)
}

/// An untracked, non-ignored file (or symlink) occupying an ancestor directory slot of `path`,
/// if any. A file->directory checkout removes such a file via `ensure_parents`, so a no-force
/// checkout must refuse when it is untracked and not `.gitignore`d; a tracked ancestor is
/// validated by the removal loop, and an ignored one is expendable.
fn untracked_file_ancestor(
	root: &Path,
	path: &str,
	tracked: &HashSet<&str>,
) -> Result<Option<String>, WorktreeError> {
	let mut ancestor = String::new();
	let mut components = path.split('/').peekable();
	while let Some(component) = components.next() {
		if components.peek().is_none() {
			break; // `path` itself, not an ancestor
		}
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(component);
		match std::fs::symlink_metadata(root.join(&ancestor)) {
			// A file/symlink occupies this ancestor; deeper components cannot exist beyond it.
			Ok(meta) if !meta.is_dir() || meta.is_symlink() => {
				if tracked.contains(ancestor.as_str()) {
					return Ok(None); // tracked: validated by the removal loop
				}
				let stack = ignore_prefix(root, &ancestor)?;
				let ignored = ignore::is_ignored(&ancestor, false, &stack);
				return Ok((!ignored).then(|| ancestor.clone()));
			}
			Ok(_) => {}
			Err(_) => return Ok(None),
		}
	}
	Ok(None)
}

/// Build the ignore stack for the ancestors of `dir_rel` — the work-tree root's `.gitignore`
/// and that of each directory strictly above `dir_rel` — ready for matching paths at `dir_rel`.
fn ignore_prefix(root: &Path, dir_rel: &str) -> Result<Vec<DirIgnore>, WorktreeError> {
	let mut stack = Vec::new();
	push_gitignore(root, "", &mut stack)?;
	let components: Vec<&str> = dir_rel.split('/').filter(|part| !part.is_empty()).collect();
	let mut ancestor = String::new();
	for component in &components[..components.len().saturating_sub(1)] {
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(component);
		push_gitignore(&root.join(&ancestor), &ancestor, &mut stack)?;
	}
	Ok(stack)
}

/// Push `dir_path`'s `.gitignore` (parsed relative to `dir_rel`) onto `stack`, returning whether
/// one was present.
fn push_gitignore(
	dir_path: &Path,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
) -> Result<bool, WorktreeError> {
	match std::fs::read_to_string(dir_path.join(".gitignore")) {
		Ok(text) => {
			stack.push(ignore::parse(&text, dir_rel));
			Ok(true)
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(error) => Err(error.into()),
	}
}

/// Remove whatever currently occupies `full` — a file, a symlink, or a whole directory —
/// leaving nothing behind, so a new entry can be written in its place.
fn clear_dest(full: &Path) -> Result<(), WorktreeError> {
	match std::fs::symlink_metadata(full) {
		Ok(meta) if meta.is_dir() && !meta.is_symlink() => std::fs::remove_dir_all(full)?,
		Ok(_) => std::fs::remove_file(full)?,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}
	Ok(())
}

fn remove_empty_parents(root: &Path, path: &str) {
	let mut full = root.join(path);
	while let Some(parent) = full.parent() {
		if parent == root || std::fs::remove_dir(parent).is_err() {
			break;
		}
		full = parent.to_path_buf();
	}
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> std::io::Result<()> {
	std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(target: &str, link: &Path) -> std::io::Result<()> {
	std::fs::write(link, target)
}

#[cfg(unix)]
fn set_mode(full: &Path, mode: &str) {
	use std::os::unix::fs::PermissionsExt;
	let perm = if mode == "100755" { 0o755 } else { 0o644 };
	let _ = std::fs::set_permissions(full, std::fs::Permissions::from_mode(perm));
}

#[cfg(not(unix))]
fn set_mode(_full: &Path, _mode: &str) {}
