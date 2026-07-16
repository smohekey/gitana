//! Restore specific paths into the working tree and/or the index (the `git restore` family).
//!
//! Unlike `checkout`, which materialises a whole tree and prunes anything absent from it,
//! `restore` only touches the paths a pathspec selects and never moves `HEAD`. Two independent
//! targets may be written: the working tree (`worktree`) and the index (`staged`). The content
//! comes from `source` — a tree, or `None` for the current index. A selected path present in the
//! source is written to the chosen targets; a selected path absent from the source but currently
//! tracked is *removed* from them (e.g. `git restore --staged` unstaging a freshly added file).
//!
//! Like stock Git's path restore, this discards uncommitted working-tree changes to the selected
//! paths — that is the operation's purpose — so there is no dirty-file guard here (that guard
//! belongs to branch switching, not path restore).

use std::collections::BTreeSet;

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::checkout::{remove_worktree_path, validate_path, write_worktree_file};
use crate::fsmeta::stat_of;
use crate::pathspec::normalize;
use crate::{IndexEntry, Stat, WorkTree, WorktreeError};

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	source: Option<ObjectId<H>>,
	worktree: bool,
	staged: bool,
	pathspecs: &[&str],
	prefix: &str,
	require_match: bool,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let mut index = wt.load_index().await?;

	// The `(path, mode, oid)` content a selected path is restored from: the source tree, or the
	// current index when there is no tree source.
	let source_entries: Vec<(String, String, ObjectId<H>)> = match source {
		Some(tree) => wt.repository().read_tree(tree).await?,
		None => index
			.entries
			.iter()
			.filter(|e| e.stage == 0)
			.map(|e| (e.path.clone(), format!("{:o}", e.mode), e.oid))
			.collect(),
	};

	// The paths a pathspec may select: everything in the source, plus every currently-tracked
	// index path. The latter lets a path that exists now but not in the source be matched for
	// removal (the worktree file deleted, the index entry dropped).
	let index_paths: Vec<String> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| e.path.clone())
		.collect();
	let mut universe: BTreeSet<&str> = source_entries.iter().map(|(p, _, _)| p.as_str()).collect();
	universe.extend(index_paths.iter().map(String::as_str));

	// Select the paths each pathspec matches. With `require_match`, a pathspec that matches
	// nothing is an error (`restore`/`checkout`); without it, an unmatched pathspec is silently
	// skipped (`reset`, which treats `reset -- <untracked-or-missing>` as a no-op, like git).
	// Either way the pathspec is still normalised, so an unsafe/empty/absolute spec is rejected.
	let mut selected: BTreeSet<&str> = BTreeSet::new();
	for &spec in pathspecs {
		let (normalized, dir_only) = normalize(spec, prefix)?;
		let mut matched = false;
		for &path in &universe {
			if matches(path, &normalized, dir_only) {
				selected.insert(path);
				matched = true;
			}
		}
		if require_match && !matched {
			return Err(WorktreeError::PathspecMatch(spec.to_owned()));
		}
	}

	// Validate every selected path before mutating anything. Source-tree and index paths bypass
	// the `normalize` guard that only sanitises the user's pathspec, so a hostile source — a
	// crafted tree or index entry such as `../x` or `.git/config` — could otherwise be upserted
	// into the index (staged restore never reaches `write_worktree_file`'s check) or used to
	// delete a file outside the work tree. Failing here also leaves nothing half-applied: the
	// working tree is untouched and the index is only saved on success.
	for &path in &selected {
		validate_path(path)?;
	}

	// Take the index lock before mutating the working tree, so a held lock aborts before any change
	// (as `rm`/`checkout` do) rather than after `save_index`. On a mid-apply failure the lock is
	// released (not orphaned) and the index is left unwritten.
	let lock = wt.lock_index().await?;
	let result: Result<(), WorktreeError> = async {
		for &path in &selected {
			match source_entries.iter().find(|(p, _, _)| p == path) {
				// Present in the source: write the chosen targets from it.
				Some((_, mode, oid)) => {
					if staged {
						// Drop entries whose shape conflicts with recording `path` as a file, the way
						// `git add` rewrites the index for a type change.
						index.remove_type_conflicts(path);
					}
					if worktree {
						write_worktree_file(wt, path, mode, *oid).await?;
					}
					if staged {
						// A staged-only restore leaves no fresh working-tree file to stat, so use a
						// default stat the worktree can never match — forcing `status` to re-hash and
						// report the entry correctly against the working tree.
						let stat = if worktree {
							let meta = wt.work().lstat(path)?.ok_or_else(|| {
								std::io::Error::new(std::io::ErrorKind::NotFound, "restored entry is missing")
							})?;
							stat_of(&meta)
						} else {
							Stat::default()
						};
						index.upsert(IndexEntry {
							stat,
							mode: u32::from_str_radix(mode, 8).unwrap_or(0o100644),
							oid: *oid,
							stage: 0,
							assume_valid: false,
							skip_worktree: false,
							path: path.to_owned(),
						});
					}
				}
				// Absent from the source but tracked: remove it from the chosen targets.
				None => {
					if worktree {
						remove_worktree_path(wt, path)?;
					}
					if staged {
						index.remove(path);
					}
				}
			}
		}
		Ok(())
	}
	.await;
	match result {
		Ok(()) => wt.commit_index(lock, &index).await,
		Err(error) => {
			wt.release_index_lock(lock).await;
			Err(error)
		}
	}
}

/// Whether `path` is matched by an already-normalised `spec`. The empty spec matches
/// everything; otherwise it matches any path beneath it as a directory, and — unless the spec
/// required a directory (`dir_only`, a trailing slash) — the path exactly.
fn matches(path: &str, spec: &str, dir_only: bool) -> bool {
	if spec.is_empty() {
		return true;
	}
	(!dir_only && path == spec) || path.starts_with(&format!("{spec}/"))
}
