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
use gitana_file_store_local::WorkDirFs;
use gitana_object::HashAlgorithm;

use crate::checkout::{remove_worktree_file, validate_path};
use crate::fsmeta::blob_of;
use crate::pathspec::PathspecSet;
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

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	pathspecs: &[&str],
	prefix: &str,
	cached: bool,
	force: bool,
	recursive: bool,
	dry_run: bool,
) -> Result<RmOutcome, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let mut index = wt.load_index().await?;
	// Tracked = stage-0 entries plus unmerged paths (which have only stage 1/2/3 entries); removing
	// an unmerged path is a valid way to resolve its conflict. An **out-of-cone** entry is excluded from
	// the tracked universe — git treats a path outside the sparse-checkout definition as outside the
	// pathspec, so a broad `rm .` never stages its deletion, and an explicit `rm <sparse>` does not match
	// (git refuses it unless `--sparse`). Filter by the active matcher, NOT just the skip-worktree bit:
	// reapply CLEARS the bit on a modified out-of-cone file it leaves in place, yet that file is still
	// outside the definition — probed vs git 2.50.1, `rm -r .` there preserves it (it would otherwise be
	// deleted and staged, losing the local content).
	let sparse = wt.sparse_checkout().await?;
	let mut tracked: Vec<String> = index
		.entries
		.iter()
		.filter(|e| {
			e.stage == 0
				&& !e.skip_worktree
				&& sparse
					.as_ref()
					.is_none_or(|matcher| matcher.includes(&e.path))
		})
		.map(|e| e.path.clone())
		.collect();
	tracked.extend(index.unmerged_paths().map(str::to_owned));

	// Match each pathspec against tracked paths: an exact file, or — needing `recursive` — the
	// contents of a directory (the empty spec from `.` matches every tracked path). A `:(exclude)`
	// pathspec subtracts from what the positives select.
	let set = PathspecSet::parse(pathspecs, prefix)?;
	let mut selected: BTreeSet<&str> = BTreeSet::new();
	for (spec, pathspec) in set.positives() {
		let mut matched = false;
		// A pathspec requires `-r` only when every match was a leading-directory expansion; if it also
		// matched a path as a plain file (exact or glob), `-r` is waived. `rm 'a?'` with tracked `a?/f`
		// AND `aa` removes both without `-r`, while `rm 'a?'` matching only the directory `a?/` requires it
		// (probed vs git 2.50.1).
		let mut has_dir_expansion = false;
		let mut has_file_match = false;
		for path in &tracked {
			if pathspec.matches(path) {
				// git decides whether a positive matched, and whether it names a recursive directory,
				// *before* subtracting exclusions — so `rm -r a :!a` is a no-op success, not "did not
				// match". Only the actual selection is gated by the negatives.
				matched = true;
				// A pathspec that expands a leading directory to its contents requires `-r`; an exact file
				// match or a glob file match does not. This holds for a literal *and* a wildcard spec whose
				// literal spelling names a directory — `rm 'a?'` on the directory `a?/` needs `-r`, `rm 'a?/f'`
				// selecting `ax/f` does not (probed vs git 2.50.1).
				if pathspec.expands_directory(path) {
					has_dir_expansion = true;
				} else {
					has_file_match = true;
				}
				if !set.is_excluded(path) {
					selected.insert(path);
				}
			}
		}
		if !matched {
			return Err(WorktreeError::PathspecMatch(spec.to_owned()));
		}
		if has_dir_expansion && !has_file_match && !recursive {
			return Err(WorktreeError::RecursiveRequired(spec.to_owned()));
		}
	}
	// With only negative pathspecs (`rm :!keep`), git applies them to an implicit `.` *relative to the
	// invocation prefix* — so it selects every tracked path under `prefix` that is not excluded, and
	// still requires `-r`, exactly as `rm .` does. A *truly* empty pathspec list is not this case: it
	// specifies nothing to remove, so it is a no-op (never the implicit `.`).
	if set.is_positive_empty() && !pathspecs.is_empty() {
		let under_prefix =
			|path: &str| prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"));
		// The implicit `.` matches every tracked path under the prefix *before* exclusions, so recursion is
		// required whenever any such path exists — even if the negatives exclude them all (`rm :!a` in a
		// repo tracking only `a` still needs `-r`).
		let mut implicit_matched = false;
		for path in &tracked {
			if under_prefix(path) {
				implicit_matched = true;
				if !set.is_excluded(path) {
					selected.insert(path.as_str());
				}
			}
		}
		// The implicit `.` matched nothing — an empty repository, or `-C sub` with every tracked path
		// elsewhere. git reports the (first) negative pathspec as unmatched rather than succeeding as a
		// no-op (probed vs git 2.50.1: `rm :!x` in an empty repo → "did not match any files").
		if !implicit_matched {
			let spec = pathspecs.first().copied().unwrap_or(".").to_owned();
			return Err(WorktreeError::PathspecMatch(spec));
		}
		if !recursive {
			return Err(WorktreeError::RecursiveRequired(".".to_owned()));
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
			// Already gone from the working tree — nothing to lose by dropping the index entry.
			let Some(meta) = wt.work().lstat(path)? else {
				continue;
			};

			// `local`: the working-tree file differs from the index entry. A submodule (gitlink) mount is
			// opaque — never hashed as a blob. Only an ABSENT (handled above) or EMPTY mount directory is
			// reconstructable, so `rm sub` removes it and its index entry without --force (probed vs git 2.55).
			// Anything else at the slot is a local modification a non-force `rm` must refuse: a POPULATED mount
			// holds the submodule's own working tree (git refuses too — via a .gitmodules name lookup, out of
			// scope here), and a regular FILE/symlink the user put where the gitlink was is local data git
			// likewise reports modified. An unreadable mount is treated as populated (fail-safe).
			let local = if entry.mode == 0o160000 {
				if meta.kind.is_dir() {
					wt.work()
						.read_dir(path)
						.map(|entries| !entries.is_empty())
						.unwrap_or(true)
				} else {
					true
				}
			} else if stat_matches(entry, &meta) {
				false
			} else {
				match blob_of(wt.work(), path, &meta)? {
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
		// Choose the removal by what actually occupies the slot (probed vs git 2.55), NOT by an index stage:
		// a gitlink materialized as a mount DIRECTORY is removed by `rmdir` (an EMPTY mount goes; a non-empty
		// one errors and keeps its index entry — never `remove_file`, which fails on a directory), while a
		// regular FILE/symlink at the slot — e.g. a mixed blob-vs-gitlink conflict whose worktree side is a
		// file — is unlinked like any other tracked path. A failure keeps the index entry so the index stays
		// consistent with the working tree.
		let is_gitlink = index
			.entries
			.iter()
			.any(|entry| entry.path == *path && entry.mode == 0o160000);
		if !cached {
			let removal = match wt.work().lstat(path)? {
				Some(meta) if is_gitlink && meta.kind.is_dir() => {
					wt.work().remove_dir(path).map_err(WorktreeError::from)
				}
				_ => remove_worktree_file(wt, path),
			};
			if let Err(error) = removal {
				failure.get_or_insert(error);
				continue;
			}
		}
		index.remove(path);
		removed.push(path.clone());
	}
	wt.commit_index(lock, &index).await?;
	Ok(RmOutcome { removed, failure })
}
