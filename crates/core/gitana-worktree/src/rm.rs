//! Remove tracked paths from the index and (unless `cached`) the working tree (`git rm`).
//!
//! Pathspecs match against *tracked* paths (index entries), not the working tree, and a spec
//! that matches nothing is an error. A spec that matches a directory's contents needs
//! `recursive`. Before anything is removed, git's data-safety check (`check_local_mod`) refuses
//! to destroy changes that are not recoverable, unless `force`:
//!
//! - `local` (working tree differs from the index) and `staged` (index differs from `HEAD`)
//!   together → refuse, in either mode (the index content matches neither side).
//! - otherwise, a full `rm` refuses a `staged`-only or `local`-only change; `cached` (which keeps
//!   the working-tree file) permits a single-sided change.
//! - a working-tree file that is already gone is always safe to drop from the index.

use std::collections::BTreeSet;

use gitana_file_store::FileStore;
use gitana_object::HashAlgorithm;

use crate::checkout::{remove_worktree_file, validate_path};
use crate::fsmeta::blob_of;
use crate::pathspec::normalize;
use crate::status::head_entries;
use crate::worktree::stat_matches;
use crate::{WorkTree, WorktreeError};

/// The outcome of [`crate::WorkTree::rm`]: the paths removed (from the index, and from the working
/// tree unless `cached` — or those that *would* be removed, under `dry_run`), and the first
/// per-path working-tree removal failure, if any. A failure leaves the successful removals applied
/// — the index stays consistent with the working tree — so the caller reports the removed paths
/// and then surfaces the error, rather than hiding the side effects behind it.
pub struct RmOutcome {
	pub removed: Vec<String>,
	pub failure: Option<WorktreeError>,
}

pub(crate) async fn run<F, H>(
	wt: &WorkTree<F, H>,
	pathspecs: &[&str],
	prefix: &str,
	cached: bool,
	force: bool,
	recursive: bool,
	dry_run: bool,
) -> Result<RmOutcome, WorktreeError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	let mut index = wt.load_index().await?;
	// Tracked = stage-0 entries plus unmerged paths (which have only stage 1/2/3 entries); removing
	// an unmerged path is a valid way to resolve its conflict.
	let mut tracked: Vec<String> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| e.path.clone())
		.collect();
	tracked.extend(index.unmerged_paths().map(str::to_owned));

	// Match each pathspec against tracked paths: an exact file, or — needing `recursive` — the
	// contents of a directory (the empty spec from `.` matches every tracked path).
	let mut selected: BTreeSet<&str> = BTreeSet::new();
	for &spec in pathspecs {
		let (normalized, dir_only) = normalize(spec, prefix)?;
		let mut exact = false;
		let mut under_dir = false;
		for path in &tracked {
			if !dir_only && path.as_str() == normalized {
				selected.insert(path);
				exact = true;
			} else if normalized.is_empty() || path.starts_with(&format!("{normalized}/")) {
				selected.insert(path);
				under_dir = true;
			}
		}
		if !exact && !under_dir {
			return Err(WorktreeError::PathspecMatch(spec.to_owned()));
		}
		if under_dir && !recursive {
			return Err(WorktreeError::RecursiveRequired(spec.to_owned()));
		}
	}

	// Validate every selected path before mutating anything: a crafted index entry such as
	// `.git/config` or `../victim` must not be deleted from the working tree (`restore` guards
	// the same way).
	for &path in &selected {
		validate_path(path)?;
	}

	if !force {
		let head = head_entries(wt).await?;
		for &path in &selected {
			// An unmerged path (no stage-0 entry) is always removable — the removal resolves the
			// conflict, as `git rm` allows without `--force`.
			let Some(entry) = index.entry(path) else {
				continue;
			};
			let full = wt.work_dir().join(path);
			let meta = match std::fs::symlink_metadata(&full) {
				Ok(meta) => meta,
				// Already gone from the working tree — nothing to lose by dropping the index entry.
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
				Err(error) => return Err(error.into()),
			};

			// `local`: the working-tree file differs from the index entry.
			let local = if stat_matches(entry, &meta) {
				false
			} else {
				match blob_of(&full, &meta)? {
					Some((oid, mode)) => oid != entry.oid || mode != entry.mode,
					None => true,
				}
			};
			// `staged`: the index entry differs from `HEAD` (an entry absent from HEAD — including
			// every entry on an unborn branch — is a staged addition).
			let staged = match head.get(path) {
				Some((mode, oid)) => *oid != entry.oid || *mode != format!("{:o}", entry.mode),
				None => true,
			};

			if local && staged {
				return Err(WorktreeError::RmStagedAndLocal(path.to_owned()));
			}
			if !cached {
				if staged {
					return Err(WorktreeError::RmStagedChanges(path.to_owned()));
				}
				if local {
					return Err(WorktreeError::RmLocalModifications(path.to_owned()));
				}
			}
		}
	}

	let selected: Vec<String> = selected.iter().map(|&p| p.to_owned()).collect();
	if dry_run {
		return Ok(RmOutcome {
			removed: selected,
			failure: None,
		});
	}

	// Take the index lock before any destructive work, so a held lock fails the command before a
	// file is deleted — and the index write at the end cannot fail for being locked, which would
	// otherwise leave files removed from the working tree but still tracked.
	let lock = wt.lock_index().await?;

	// Per-path, as git does: drop the index entry only for a path whose working-tree file was
	// removed (or `cached`). A path whose file cannot be unlinked — a directory now occupies it,
	// its parent is not writable — keeps its entry, so the index stays consistent with the working
	// tree (never half-applied). Successful removals are still reported when a later path fails.
	let mut removed = Vec::with_capacity(selected.len());
	let mut failure: Option<WorktreeError> = None;
	for path in &selected {
		if !cached && let Err(error) = remove_worktree_file(wt, path) {
			failure.get_or_insert(error);
			continue;
		}
		index.remove(path);
		removed.push(path.clone());
	}
	wt.commit_index(lock, &index).await?;
	Ok(RmOutcome { removed, failure })
}
