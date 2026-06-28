//! Three-way status: HEAD tree vs index (staged) and index vs working tree
//! (unstaged), plus untracked files. Untracked detection applies `.gitignore` and
//! collapses fully-untracked directories the way git's default mode does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use gitana_file_store::FileStore;
use gitana_object::ObjectId;

use crate::fsmeta::blob_of;
use crate::ignore::{self, DirIgnore};
use crate::worktree::stat_matches;
use crate::{IndexEntry, WorkTree, WorktreeError};

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

/// The status of a working tree: changed and untracked paths, sorted by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
	/// Entries with a non-clean index or worktree code, or untracked.
	pub entries: Vec<StatusEntry>,
}

impl Status {
	/// Render in `git status --porcelain=v1` form (`XY path` per line).
	pub fn porcelain_v1(&self) -> String {
		let mut out = String::new();
		for entry in &self.entries {
			out.push(entry.index);
			out.push(entry.worktree);
			out.push(' ');
			out.push_str(&entry.path);
			out.push('\n');
		}
		out
	}
}

pub(crate) async fn compute<F: FileStore>(wt: &WorkTree<F>) -> Result<Status, WorktreeError> {
	let index = wt.load_index()?;
	let index_map: HashMap<String, (String, ObjectId)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();
	let head_map = head_entries(wt).await?;

	let mut untracked = Vec::new();
	let mut ignore_stack: Vec<DirIgnore> = Vec::new();
	collect_untracked(
		wt.work_dir(),
		"",
		&index_map,
		&mut ignore_stack,
		&mut untracked,
	)?;

	let mut merged: BTreeMap<String, StatusEntry> = BTreeMap::new();

	// Index vs HEAD (the X column).
	let all: BTreeSet<&String> = index_map.keys().chain(head_map.keys()).collect();
	for path in all {
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
		let code = worktree_change(entry, &wt.work_dir().join(&entry.path))?;
		if code != ' ' {
			at(&mut merged, &entry.path).worktree = code;
		}
	}

	// Untracked working-tree files and collapsed untracked directories.
	for path in untracked {
		let slot = at(&mut merged, &path);
		slot.index = '?';
		slot.worktree = '?';
	}

	Ok(Status {
		entries: merged.into_values().collect(),
	})
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

async fn head_entries<F: FileStore>(
	wt: &WorkTree<F>,
) -> Result<HashMap<String, (String, ObjectId)>, WorktreeError> {
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
fn collect_untracked(
	dir_path: &Path,
	dir_rel: &str,
	index_map: &HashMap<String, (String, ObjectId)>,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
) -> Result<(), WorktreeError> {
	let pushed = match std::fs::read_to_string(dir_path.join(".gitignore")) {
		Ok(text) => {
			stack.push(ignore::parse(&text, dir_rel));
			true
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => return Err(error.into()),
	};

	for entry in std::fs::read_dir(dir_path)? {
		let entry = entry?;
		let name = entry.file_name();
		let name = name.to_string_lossy();
		if name == ".git" {
			continue;
		}
		let rel = if dir_rel.is_empty() {
			name.into_owned()
		} else {
			format!("{dir_rel}/{name}")
		};
		// `DirEntry::metadata` does not traverse symlinks (like `lstat`).
		let is_dir = entry.metadata()?.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue;
		}
		if is_dir {
			let prefix = format!("{rel}/");
			if index_map.keys().any(|path| path.starts_with(&prefix)) {
				collect_untracked(&entry.path(), &rel, index_map, stack, out)?;
			} else {
				out.push(prefix); // fully-untracked directory → "dir/"
			}
		} else if !index_map.contains_key(&rel) {
			out.push(rel);
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}

fn worktree_change(entry: &IndexEntry, full: &Path) -> Result<char, WorktreeError> {
	let meta = match std::fs::symlink_metadata(full) {
		Ok(meta) => meta,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok('D'),
		Err(error) => return Err(error.into()),
	};
	if stat_matches(entry, &meta) {
		return Ok(' ');
	}
	match blob_of(full, &meta)? {
		Some((oid, mode)) if oid == entry.oid && mode == entry.mode => Ok(' '),
		_ => Ok('M'),
	}
}
