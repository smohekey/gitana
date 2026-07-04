//! Gather the content changes for `diff`: index-vs-working-tree (unstaged) and
//! HEAD-vs-index (staged). This collects the before/after bytes and modes for each
//! changed path; line-diffing and unified-diff formatting are the caller's job
//! (gta owns its output, per docs/hlds/gta-cli.md).

use std::collections::{BTreeMap, BTreeSet};

use gitana_file_store::FileStore;
use gitana_file_store_local::{Meta, WorkDirFs};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::fsmeta::{blob_of, effective_mode};
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
pub(crate) async fn unstaged<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Vec<FileDiff>, WorktreeError> {
	let index = wt.load_index().await?;
	let mut out = Vec::new();
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		let Some(meta) = wt.work().lstat(&entry.path)? else {
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
		match blob_of(wt.work(), &entry.path, &meta)? {
			Some((oid, _)) if oid == entry.oid && effective_mode(&meta, entry.mode) == entry.mode => {}
			_ => {
				let old = wt.repository().read_blob(entry.oid).await?;
				let new = read_worktree(wt.work(), &entry.path, &meta)?;
				out.push(FileDiff {
					path: entry.path.clone(),
					old: Some((old, entry.mode)),
					// The new-side mode is the *effective* mode: under a capability that cannot report the
					// executable bit (WASI), it inherits the bit from the index entry, so a content-only
					// edit of an executable does not print a spurious `100755 → 100644` mode change —
					// git's `core.fileMode=false`.
					new: Some((new, effective_mode(&meta, entry.mode))),
				});
			}
		}
	}
	out.sort_by(|a, b| a.path.cmp(&b.path));
	Ok(out)
}

/// HEAD tree vs index (the changes `git diff --cached` shows).
pub(crate) async fn staged<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Vec<FileDiff>, WorktreeError> {
	let index: BTreeMap<String, (u32, ObjectId<H>)> = wt
		.load_index()
		.await?
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (e.mode, e.oid)))
		.collect();

	let head: BTreeMap<String, (u32, ObjectId<H>)> =
		match wt.repository().refs().resolve_head().await? {
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
			old: side(wt.repository(), old).await?,
			new: side(wt.repository(), new).await?,
		});
	}
	Ok(out)
}

/// The content changes between two tree objects (`old` → `new`), for showing a commit or a
/// tree-to-tree diff. `old` is `None` for an empty left side (a root commit has no parent) — an
/// empty side is represented in memory, never materialised, so this stays read-only. Needs only the
/// repository — no working tree — but produces the same [`FileDiff`]s as the index/working-tree
/// diffs, so it lives alongside them. Submodule (gitlink) entries are skipped: no blob to diff.
pub async fn trees<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	old: Option<ObjectId<H>>,
	new: ObjectId<H>,
) -> Result<Vec<FileDiff>, WorktreeError> {
	let old = match old {
		Some(tree) => tree_entries(repo, tree).await?,
		None => BTreeMap::new(),
	};
	let new = tree_entries(repo, new).await?;
	let paths: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
	let mut out = Vec::new();
	for path in paths {
		let (o, n) = (old.get(path), new.get(path));
		if o == n {
			continue;
		}
		out.push(FileDiff {
			path: path.clone(),
			old: side(repo, o).await?,
			new: side(repo, n).await?,
		});
	}
	Ok(out)
}

/// A tree flattened to `path -> (mode, oid)`, dropping gitlinks (submodule entries).
async fn tree_entries<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	tree: ObjectId<H>,
) -> Result<BTreeMap<String, (u32, ObjectId<H>)>, WorktreeError> {
	Ok(
		repo
			.read_tree(tree)
			.await?
			.into_iter()
			.filter(|(_, mode, _)| mode != "160000")
			.map(|(path, mode, oid)| (path, (parse_mode(&mode), oid)))
			.collect(),
	)
}

/// Resolve a `(mode, oid)` reference to its blob content and mode, or `None`.
async fn side<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	what: Option<&(u32, ObjectId<H>)>,
) -> Result<Option<(Vec<u8>, u32)>, WorktreeError> {
	match what {
		Some((mode, oid)) => Ok(Some((repo.read_blob(*oid).await?, *mode))),
		None => Ok(None),
	}
}

/// Read a working-tree path's blob bytes: the file contents, or a symlink's target.
fn read_worktree<W: WorkDirFs>(work: &W, path: &str, meta: &Meta) -> std::io::Result<Vec<u8>> {
	if meta.kind.is_symlink() {
		work.read_link(path)
	} else {
		work.read(path)
	}
}

fn parse_mode(mode: &str) -> u32 {
	u32::from_str_radix(mode, 8).unwrap_or(0o100644)
}
