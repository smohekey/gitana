//! Move/rename tracked paths: a filesystem rename plus an index update (`git mv`).
//!
//! Each source must be tracked (a file, or a directory whose entries are tracked) and present in
//! the working tree. The destination is either a rename target (single source, not an existing
//! directory) or a directory the sources move into (multiple sources, a trailing slash, or an
//! existing directory). The destination must not already exist unless `force`. Everything is
//! validated before anything moves; then, like `rm`, the index lock is taken before the first
//! filesystem change so a held lock or a bad plan never leaves a half-applied move.

use std::collections::BTreeSet;
use std::path::Path;

use gitana_file_store::FileStore;
use gitana_object::HashAlgorithm;

use crate::checkout::validate_path;
use crate::fsmeta::{blob_of, stat_of};
use crate::pathspec::normalize;
use crate::{IndexEntry, Stat, WorkTree, WorktreeError};

pub(crate) async fn run<F, H>(
	wt: &WorkTree<F, H>,
	sources: &[&str],
	dest: &str,
	prefix: &str,
	force: bool,
	dry_run: bool,
) -> Result<Vec<(String, String)>, WorktreeError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	let mut index = wt.load_index()?;
	let tracked: Vec<String> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| e.path.clone())
		.collect();

	let (dest_norm, dest_trailing_slash) = normalize(dest, prefix)?;
	let dest_is_dir = matches!(
		std::fs::symlink_metadata(wt.work_dir().join(&dest_norm)),
		Ok(meta) if meta.is_dir() && !meta.is_symlink()
	);
	// A directory target: several sources, an explicit trailing slash, or an existing directory.
	let into_dir = sources.len() > 1 || dest_trailing_slash || dest_is_dir;
	if into_dir && !dest_is_dir {
		return Err(WorktreeError::MvDestinationNotDir(dest.to_owned()));
	}

	// Plan every move and validate it before touching the filesystem.
	let mut moves: Vec<(String, String)> = Vec::with_capacity(sources.len());
	let mut planned: BTreeSet<String> = BTreeSet::new();
	for &src in sources {
		let (src_norm, _) = normalize(src, prefix)?;
		let dir_prefix = format!("{src_norm}/");
		let is_file = tracked.contains(&src_norm);
		let is_dir = tracked.iter().any(|p| p.starts_with(&dir_prefix));
		if !is_file && !is_dir {
			return Err(WorktreeError::MvSourceUntracked(src.to_owned()));
		}
		// Validate the source before treating it as a path to rename: a crafted index entry such
		// as `.git/config` must not be moved (`rm`/`restore` guard their selected paths the same
		// way). The destination side is validated below, including each remapped sub-entry.
		validate_path(&src_norm)?;
		if std::fs::symlink_metadata(wt.work_dir().join(&src_norm)).is_err() {
			return Err(WorktreeError::MvBadSource(src.to_owned()));
		}

		let dst = if into_dir {
			let base = src_norm.rsplit('/').next().unwrap_or(&src_norm);
			if dest_norm.is_empty() {
				base.to_owned()
			} else {
				format!("{dest_norm}/{base}")
			}
		} else {
			dest_norm.clone()
		};
		validate_path(&dst)?;
		// Validate every index path the move will create under a directory source, before any
		// filesystem change.
		for path in &tracked {
			if let Some(rest) = path.strip_prefix(&dir_prefix) {
				validate_path(&format!("{dst}/{rest}"))?;
			}
		}

		if dst == src_norm || dst.starts_with(&dir_prefix) {
			return Err(WorktreeError::MvIntoSelf(src.to_owned()));
		}
		if !force && std::fs::symlink_metadata(wt.work_dir().join(&dst)).is_ok() {
			return Err(WorktreeError::MvDestinationExists(dst));
		}
		if let Some(parent) = Path::new(&dst)
			.parent()
			.filter(|p| !p.as_os_str().is_empty())
			&& !wt.work_dir().join(parent).is_dir()
		{
			return Err(WorktreeError::MvDestinationDirMissing(dst));
		}
		if !planned.insert(dst.clone()) {
			return Err(WorktreeError::MvDuplicateDestination(dst));
		}
		moves.push((src_norm, dst));
	}

	if dry_run {
		return Ok(moves);
	}

	// Lock the index before the first rename, so a held lock aborts before any file moves.
	let lock = wt.lock_index()?;
	for (src_norm, dst) in &moves {
		let step = std::fs::rename(wt.work_dir().join(src_norm), wt.work_dir().join(dst))
			.map_err(WorktreeError::from)
			.and_then(|()| reindex(wt, &mut index, src_norm, dst));
		// On a mid-move failure, release the lock so it is not left stale; the moves already
		// applied stay (git's `mv` is likewise not atomic across a rename failure).
		if let Err(error) = step {
			wt.release_index_lock(lock);
			return Err(error);
		}
	}
	wt.commit_index(lock, &index)?;
	Ok(moves)
}

/// Move `src`'s index entries to `dst`: a single file entry, or every entry under `src/` with its
/// path re-prefixed. The blob id and mode are kept (the content did not change); see [`remap`] for
/// how the stat cache is handled.
fn reindex<F, H>(
	wt: &WorkTree<F, H>,
	index: &mut crate::Index<H>,
	src: &str,
	dst: &str,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	if let Some(entry) = index.entry(src).cloned() {
		let moved = remap(wt, entry, dst)?;
		index.remove(src);
		index.upsert(moved);
		return Ok(());
	}

	let dir_prefix = format!("{src}/");
	let entries: Vec<IndexEntry<H>> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0 && e.path.starts_with(&dir_prefix))
		.cloned()
		.collect();
	for entry in entries {
		let new_path = format!("{dst}{}", &entry.path[src.len()..]);
		let old_path = entry.path.clone();
		let moved = remap(wt, entry, &new_path)?;
		index.remove(&old_path);
		index.upsert(moved);
	}
	Ok(())
}

/// Clone `entry` to live at `new_path`. The blob id and mode are kept (the content moved
/// unchanged). The stat cache is refreshed from the moved file only if its content still matches
/// the staged blob; otherwise a default stat is left, so a moved-but-dirty file is not hidden — it
/// cannot match the cache, forcing `status` to re-hash and report the unstaged modification.
fn remap<F, H>(
	wt: &WorkTree<F, H>,
	mut entry: IndexEntry<H>,
	new_path: &str,
) -> Result<IndexEntry<H>, WorktreeError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	let full = wt.work_dir().join(new_path);
	let meta = std::fs::symlink_metadata(&full)?;
	let clean = matches!(
		blob_of(&full, &meta)?,
		Some((oid, mode)) if oid == entry.oid && mode == entry.mode
	);
	entry.stat = if clean {
		stat_of(&meta)
	} else {
		Stat::default()
	};
	entry.path = new_path.to_owned();
	Ok(entry)
}
