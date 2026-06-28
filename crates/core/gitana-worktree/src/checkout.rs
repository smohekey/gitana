//! Materialise a tree into the working directory and index.
//!
//! Writes regular files (with the exec bit), symlinks, and removes files absent
//! from the target tree; updates the index to match. Without `force` it refuses to
//! overwrite uncommitted local changes. Paths are validated against traversal,
//! `.git`, and symlinked ancestors (the git checkout CVE class).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gitana_file_store::FileStore;
use gitana_object::ObjectId;

use crate::fsmeta::{blob_of, stat_of};
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
		for (path, (mode, oid)) in &target_paths {
			let differs = current
				.get(*path)
				.is_none_or(|(cm, co)| cm != mode || co != oid);
			if differs {
				ensure_no_overwrite(wt, path, current.get(*path))?;
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
			let full = wt.work_dir().join(path);
			let _ = std::fs::remove_file(&full);
			remove_empty_parents(wt.work_dir(), path);
			index.remove(path);
		}
	}

	wt.save_index(&index)
}

async fn write_entry<F>(
	wt: &WorkTree<F>,
	path: &str,
	mode: &str,
	oid: ObjectId,
	index: &mut crate::Index,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	validate_path(path)?;
	let full = ensure_parents(wt.work_dir(), path)?;
	let content = wt.repository().read_blob(oid).await?;

	if mode == "120000" {
		let _ = std::fs::remove_file(&full);
		symlink(&String::from_utf8_lossy(&content), &full)?;
	} else {
		// Never write through an existing symlink at the destination.
		if let Ok(meta) = std::fs::symlink_metadata(&full)
			&& meta.is_symlink()
		{
			std::fs::remove_file(&full)?;
		}
		std::fs::write(&full, &content)?;
		set_mode(&full, mode);
	}

	let meta = std::fs::symlink_metadata(&full)?;
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
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error.into()),
	};
	match current {
		// Tracked: a conflict only if the working file is dirty vs the index.
		Some((mode, oid)) => match blob_of(&full, &meta)? {
			Some((woid, wmode)) if woid == *oid && format!("{wmode:o}") == *mode => Ok(()),
			_ => Err(WorktreeError::Conflict(path.to_owned())),
		},
		// Untracked file in the way of a checked-out path.
		None => Err(WorktreeError::Conflict(path.to_owned())),
	}
}

fn validate_path(path: &str) -> Result<(), WorktreeError> {
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
/// symlinked ancestor, and return the full path.
fn ensure_parents(root: &Path, path: &str) -> Result<PathBuf, WorktreeError> {
	let mut full = root.to_path_buf();
	let parts: Vec<&str> = path.split('/').collect();
	for part in &parts[..parts.len().saturating_sub(1)] {
		full.push(part);
		match std::fs::symlink_metadata(&full) {
			Ok(meta) if meta.is_dir() && !meta.is_symlink() => {}
			Ok(_) => return Err(WorktreeError::UnsafePath(path.to_owned())),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				std::fs::create_dir(&full)?;
			}
			Err(error) => return Err(error.into()),
		}
	}
	full.push(parts.last().copied().unwrap_or_default());
	Ok(full)
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
