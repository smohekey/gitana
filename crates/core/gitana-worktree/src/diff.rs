//! Gather the content changes for `diff`: index-vs-working-tree (unstaged) and
//! HEAD-vs-index (staged). This collects the before/after bytes and modes for each
//! changed path; line-diffing and unified-diff formatting are the caller's job
//! (gta owns its output, per docs/hlds/gta-cli.md).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::Path;

use gitana_file_store::FileStore;
use gitana_object::ObjectId;

use crate::fsmeta::{blob_of, file_mode, path_bytes};
use crate::worktree::stat_matches;
use crate::{WorkTree, WorktreeError};

/// One path's change between two sides. A `None` content/mode means the path is
/// absent on that side (an addition or deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
	/// Repository-relative path (`/`-separated).
	pub path: String,
	/// Content and git mode on the "old" (left) side, or `None` if absent.
	pub old: Option<(Vec<u8>, u32)>,
	/// Content and git mode on the "new" (right) side, or `None` if absent.
	pub new: Option<(Vec<u8>, u32)>,
}

/// Index vs working tree (the changes `git diff` shows with no args). Untracked
/// files are not included, matching git's default.
pub(crate) async fn unstaged<F: FileStore>(
	wt: &WorkTree<F>,
) -> Result<Vec<FileDiff>, WorktreeError> {
	let index = wt.load_index()?;
	let mut out = Vec::new();
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		let full = wt.work_dir().join(&entry.path);
		let meta = match std::fs::symlink_metadata(&full) {
			Ok(meta) => Some(meta),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
			Err(error) => return Err(error.into()),
		};
		let Some(meta) = meta else {
			// Deleted in the working tree.
			let old = wt.repository().read_blob(entry.oid).await?;
			out.push(FileDiff {
				path: entry.path.clone(),
				old: Some((old, entry.mode)),
				new: None,
			});
			continue;
		};
		if stat_matches(entry, &meta) {
			continue;
		}
		// Re-hash to decide if the content really changed (mtime alone can lie).
		match blob_of(&full, &meta)? {
			Some((oid, mode)) if oid == entry.oid && mode == entry.mode => {}
			_ => {
				let old = wt.repository().read_blob(entry.oid).await?;
				let new = read_worktree(&full, &meta)?;
				out.push(FileDiff {
					path: entry.path.clone(),
					old: Some((old, entry.mode)),
					new: Some((new, working_mode(&meta))),
				});
			}
		}
	}
	out.sort_by(|a, b| a.path.cmp(&b.path));
	Ok(out)
}

/// HEAD tree vs index (the changes `git diff --cached` shows).
pub(crate) async fn staged<F: FileStore>(wt: &WorkTree<F>) -> Result<Vec<FileDiff>, WorktreeError> {
	let index: BTreeMap<String, (u32, ObjectId)> = wt
		.load_index()?
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (e.mode, e.oid)))
		.collect();

	let head: BTreeMap<String, (u32, ObjectId)> = match wt.repository().refs().resolve_head().await? {
		Some(commit) => {
			let tree = wt.repository().commit_tree(commit).await?;
			wt.repository()
				.read_tree(tree)
				.await?
				.into_iter()
				.map(|(path, mode, oid)| (path, (parse_mode(&mode), oid)))
				.collect()
		}
		None => BTreeMap::new(),
	};

	let paths: BTreeSet<&String> = index.keys().chain(head.keys()).collect();
	let mut out = Vec::new();
	for path in paths {
		let old = head.get(path);
		let new = index.get(path);
		if old == new {
			continue;
		}
		out.push(FileDiff {
			path: path.clone(),
			old: self::side(wt, old).await?,
			new: self::side(wt, new).await?,
		});
	}
	Ok(out)
}

/// Resolve a `(mode, oid)` reference to its blob content and mode, or `None`.
async fn side<F: FileStore>(
	wt: &WorkTree<F>,
	what: Option<&(u32, ObjectId)>,
) -> Result<Option<(Vec<u8>, u32)>, WorktreeError> {
	match what {
		Some((mode, oid)) => Ok(Some((wt.repository().read_blob(*oid).await?, *mode))),
		None => Ok(None),
	}
}

/// Read a working-tree path's blob bytes: the file contents, or a symlink's target.
fn read_worktree(full: &Path, meta: &Metadata) -> std::io::Result<Vec<u8>> {
	if meta.is_symlink() {
		Ok(path_bytes(&std::fs::read_link(full)?).to_vec())
	} else {
		std::fs::read(full)
	}
}

fn working_mode(meta: &Metadata) -> u32 {
	if meta.is_symlink() {
		0o120000
	} else {
		file_mode(meta)
	}
}

fn parse_mode(mode: &str) -> u32 {
	u32::from_str_radix(mode, 8).unwrap_or(0o100644)
}
