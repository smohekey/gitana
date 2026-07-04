//! Three-way status: HEAD tree vs index (staged) and index vs working tree
//! (unstaged), plus untracked files. Untracked detection applies `.gitignore` and
//! collapses fully-untracked directories the way git's default mode does.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::fsmeta::{blob_of, join_rel, push_gitignore};
use crate::ignore::{self, DirIgnore};
use crate::worktree::stat_matches;
use crate::{Conflict, IndexEntry, WorkTree, WorktreeError};

/// One path's status: an index (staged-vs-HEAD) code and a worktree
/// (unstaged-vs-index) code, using git's letters (` `, `A`, `M`, `D`, `?`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
	/// Repository-relative path.
	pub path: String,
	/// Index-vs-HEAD code (the `X` column of `git status --porcelain`).
	pub index: char,
	/// Worktree-vs-index code (the `Y` column).
	pub worktree: char,
}

/// The status of a working tree. `changed` holds tracked paths with a non-clean index/worktree code
/// (or a conflict); `untracked` holds untracked paths and collapsed untracked directories. Both are
/// sorted by path, and a path may appear in both (e.g. after `rm --cached`: a staged deletion plus
/// the still-present, now-untracked working file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
	pub changed: Vec<StatusEntry>,
	pub untracked: Vec<String>,
}

impl Status {
	/// Render in `git status --porcelain=v1` form: tracked changes first, then untracked (`?? path`),
	/// matching git's grouping rather than a single global path sort.
	pub fn porcelain_v1(&self) -> String {
		let mut out = String::new();
		for entry in &self.changed {
			out.push(entry.index);
			out.push(entry.worktree);
			out.push(' ');
			out.push_str(&entry.path);
			out.push('\n');
		}
		for path in &self.untracked {
			out.push_str("?? ");
			out.push_str(path);
			out.push('\n');
		}
		out
	}
}

pub(crate) async fn compute<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Status, WorktreeError> {
	let index = wt.load_index().await?;
	let index_map: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();
	let head_map = head_entries(wt).await?;

	// Unmerged paths carry their conflict code (`UU`/`AA`/…) rather than the normal X/Y columns.
	let unmerged: BTreeMap<String, (char, char)> = index
		.unmerged_paths()
		.map(|path| {
			let conflict = index.conflict(path).expect("unmerged path has a conflict");
			(path.to_owned(), conflict_code(&conflict))
		})
		.collect();

	// A conflicted path is tracked (it has a working-tree file), so exclude it from untracked.
	let tracked: HashSet<String> = index_map
		.keys()
		.cloned()
		.chain(unmerged.keys().cloned())
		.collect();
	let mut untracked = Vec::new();
	let mut ignore_stack: Vec<DirIgnore> = Vec::new();
	collect_untracked(wt.work(), "", &tracked, &mut ignore_stack, &mut untracked)?;

	let mut merged: BTreeMap<String, StatusEntry> = BTreeMap::new();

	// Index vs HEAD (the X column); unmerged paths are handled separately.
	let all: BTreeSet<&String> = index_map.keys().chain(head_map.keys()).collect();
	for path in all {
		if unmerged.contains_key(path) {
			continue;
		}
		let code = match (index_map.get(path), head_map.get(path)) {
			(Some(i), Some(h)) if i != h => 'M',
			(Some(_), Some(_)) => ' ',
			(Some(_), None) => 'A',
			(None, Some(_)) => 'D',
			(None, None) => ' ',
		};
		if code != ' ' {
			at(&mut merged, path).index = code;
		}
	}

	// Working tree vs index (the Y column).
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		let code = worktree_change(wt.work(), entry, &entry.path)?;
		if code != ' ' {
			at(&mut merged, &entry.path).worktree = code;
		}
	}

	// Unmerged paths: emit the two-letter conflict code as the X/Y columns.
	for (path, (x, y)) in &unmerged {
		let slot = at(&mut merged, path);
		slot.index = *x;
		slot.worktree = *y;
	}

	// Untracked paths are reported separately (git lists them after the tracked changes), so a path
	// that is both a tracked change and an untracked working file — e.g. after `rm --cached` — keeps
	// both lines instead of one clobbering the other.
	untracked.sort();

	Ok(Status {
		changed: merged.into_values().collect(),
		untracked,
	})
}

/// git's `git status --porcelain` two-letter code for an unmerged path, from which of base (1),
/// ours (2), and theirs (3) are present.
fn conflict_code<H: HashAlgorithm>(conflict: &Conflict<H>) -> (char, char) {
	match (
		conflict.base.is_some(),
		conflict.ours.is_some(),
		conflict.theirs.is_some(),
	) {
		(true, true, true) => ('U', 'U'),   // both modified
		(false, true, true) => ('A', 'A'),  // both added
		(true, true, false) => ('U', 'D'),  // deleted by them
		(true, false, true) => ('D', 'U'),  // deleted by us
		(true, false, false) => ('D', 'D'), // both deleted
		(false, true, false) => ('A', 'U'), // added by us
		(false, false, true) => ('U', 'A'), // added by them
		(false, false, false) => unreachable!("a conflict has at least one stage"),
	}
}

fn at<'a>(merged: &'a mut BTreeMap<String, StatusEntry>, path: &str) -> &'a mut StatusEntry {
	merged
		.entry(path.to_owned())
		.or_insert_with(|| StatusEntry {
			path: path.to_owned(),
			index: ' ',
			worktree: ' ',
		})
}

pub(crate) async fn head_entries<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<HashMap<String, (String, ObjectId<H>)>, WorktreeError> {
	let Some(commit) = wt.repository().refs().resolve_head().await? else {
		return Ok(HashMap::new());
	};
	let tree = wt.repository().commit_tree(commit).await?;
	Ok(
		wt.repository()
			.read_tree(tree)
			.await?
			.into_iter()
			.map(|(path, mode, oid)| (path, (mode, oid)))
			.collect(),
	)
}

/// Walk the working tree collecting untracked paths: apply `.gitignore` per
/// directory, skip ignored entries, and collapse a fully-untracked directory to a
/// single `dir/` entry (git's default untracked mode). Tracked files are skipped
/// (their worktree status is handled separately).
fn collect_untracked<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	tracked: &HashSet<String>,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
) -> Result<(), WorktreeError> {
	let pushed = push_gitignore(work, dir_rel, stack)?;

	for entry in work.read_dir(dir_rel)? {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		// The entry's kind is an `lstat` (a symlinked directory is a symlink, not a directory).
		let is_dir = entry.kind.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue;
		}
		if is_dir {
			let prefix = format!("{rel}/");
			if tracked.iter().any(|path| path.starts_with(&prefix)) {
				collect_untracked(work, &rel, tracked, stack, out)?;
			} else {
				out.push(prefix); // fully-untracked directory → "dir/"
			}
		} else if !tracked.contains(&rel) {
			out.push(rel);
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}

fn worktree_change<W: WorkDirFs, H: HashAlgorithm>(
	work: &W,
	entry: &IndexEntry<H>,
	path: &str,
) -> Result<char, WorktreeError> {
	let Some(meta) = work.lstat(path)? else {
		return Ok('D');
	};
	if stat_matches(entry, &meta) {
		return Ok(' ');
	}
	match blob_of(work, path, &meta)? {
		Some((oid, mode)) if oid == entry.oid && mode == entry.mode => Ok(' '),
		_ => Ok('M'),
	}
}

#[cfg(test)]
mod tests {
	use gitana_object::{ObjectKind, Sha256};

	use super::*;
	use crate::Index;

	/// The porcelain code for a conflict with the given stages present.
	fn code(base: bool, ours: bool, theirs: bool) -> (char, char) {
		let oid = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x");
		let stage = |present: bool| present.then_some((0o100644u32, oid));
		let mut index = Index::new();
		index.record_conflict("f", stage(base), stage(ours), stage(theirs));
		conflict_code(&index.conflict("f").unwrap())
	}

	#[test]
	fn conflict_codes_match_git() {
		assert_eq!(code(true, true, true), ('U', 'U')); // both modified
		assert_eq!(code(false, true, true), ('A', 'A')); // both added
		assert_eq!(code(true, true, false), ('U', 'D')); // deleted by them
		assert_eq!(code(true, false, true), ('D', 'U')); // deleted by us
		assert_eq!(code(true, false, false), ('D', 'D')); // both deleted
		assert_eq!(code(false, true, false), ('A', 'U')); // added by us
		assert_eq!(code(false, false, true), ('U', 'A')); // added by them
	}
}
