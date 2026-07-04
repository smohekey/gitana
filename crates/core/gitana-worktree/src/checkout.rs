//! Materialise a tree into the working directory and index.
//!
//! Writes regular files (with the exec bit), symlinks, and removes files absent
//! from the target tree; updates the index to match. Without `force` it refuses to
//! overwrite uncommitted local changes. Paths are validated against traversal,
//! `.git`, and symlinked ancestors (the git checkout CVE class).

use std::collections::{HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::fsmeta::{blob_of, join_rel, push_gitignore, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{IndexEntry, WorkTree, WorktreeError};

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	tree: ObjectId<H>,
	force: bool,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let target = wt.repository().read_tree(tree).await?;
	let target_paths: HashMap<&str, (&str, ObjectId<H>)> = target
		.iter()
		.map(|(path, mode, oid)| (path.as_str(), (mode.as_str(), *oid)))
		.collect();

	let mut index = wt.load_index().await?;
	let current: HashMap<String, (String, ObjectId<H>)> = index
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
			match wt.work().lstat(path)? {
				// A directory occupies this file's slot (a directory->file change). Replacing it
				// would delete everything under it, so refuse if it holds a non-ignored untracked
				// file; its tracked contents are validated for cleanliness by the removal loop.
				Some(meta) if meta.kind.is_dir() => {
					let mut stack = ignore_prefix(wt.work(), path)?;
					if let Some(untracked) = first_untracked_under(wt.work(), path, &tracked, &mut stack)? {
						return Err(WorktreeError::UntrackedOverwrite(untracked));
					}
				}
				_ => ensure_no_overwrite(wt, path, current.get(*path))?,
			}
			// A file->directory change removes a file occupying an ancestor slot; refuse if that
			// file is an untracked, non-ignored file (a tracked ancestor is validated by the
			// removal loop, an ignored one is expendable).
			if let Some(untracked) = untracked_file_ancestor(wt.work(), path, &tracked)? {
				return Err(WorktreeError::UntrackedOverwrite(untracked));
			}
		}
		for path in current.keys() {
			if !target_paths.contains_key(path.as_str()) {
				ensure_no_overwrite(wt, path, current.get(path))?;
			}
		}
	}

	// Take the index lock before touching the working tree, so a held lock aborts here — before any
	// filesystem change — rather than after, which would leave the tree inconsistent with the index.
	// On a mid-materialise failure the lock is released (not orphaned) and the index is left unwritten,
	// matching the pre-lock behaviour of not saving a partially-applied index.
	let lock = wt.lock_index().await?;
	let result: Result<(), WorktreeError> = async {
		// The paths the removal loop will prune: index entries (any stage) absent from the target.
		// Spanning all stages lets a force checkout (e.g. `merge --abort`) discard leftover conflict
		// stages too. Computed and lexically validated up front, before materialising anything, so a
		// hostile index path (`../x`, `.git/…`) aborts with the working tree untouched. The set is the
		// same before and after the writes, which only upsert target entries.
		let stray: Vec<String> = index
			.entries
			.iter()
			.map(|e| e.path.as_str())
			.filter(|path| !target_paths.contains_key(path))
			.collect::<std::collections::BTreeSet<_>>()
			.into_iter()
			.map(str::to_owned)
			.collect();
		for path in &stray {
			validate_path(path)?;
		}

		for (path, mode, oid) in &target {
			// Without `force`, leave a path unchanged from the index alone — so a local edit to a file
			// the checkout does not touch (e.g. an unrelated dirty file during a merge) is preserved,
			// the way git does. `force` (re)writes everything, restoring such files.
			if !force
				&& current
					.get(path)
					.is_some_and(|(cm, co)| cm == mode && co == oid)
			{
				continue;
			}
			write_entry(wt, path, mode, *oid, &mut index).await?;
		}
		// `remove_worktree_path` re-validates and declines to follow a symlinked ancestor (after a
		// directory→symlink switch the stale child under the old directory is already gone), so the
		// removal never escapes the work tree.
		for path in &stray {
			remove_worktree_path(wt, path)?;
			index.remove(path);
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

/// Apply only the `from_tree` → `to_tree` diff to the work tree and index — git's `read-tree -m -u`
/// two-way merge, used for a fast-forward. Paths unchanged between the two trees (including unrelated
/// staged or dirty entries) are left untouched. A *changed* path that is not clean — its stage-0
/// index entry differs from `from` (a staged change), its work-tree file differs from the index (a
/// local edit), or an untracked file sits where `to` adds one — would be overwritten; all such paths
/// are returned (sorted) and **nothing** is applied. An empty result means the diff was applied.
pub(crate) async fn twoway_merge<F, W, H>(
	wt: &WorkTree<F, W, H>,
	from_tree: ObjectId<H>,
	to_tree: ObjectId<H>,
) -> Result<Vec<String>, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let from = tree_map(wt.repository().read_tree(from_tree).await?);
	let to = tree_map(wt.repository().read_tree(to_tree).await?);

	// The only paths this update touches: those that differ between the two trees.
	let mut changed: Vec<&str> = from
		.keys()
		.chain(to.keys())
		.map(String::as_str)
		.collect::<HashSet<_>>()
		.into_iter()
		.filter(|path| from.get(*path) != to.get(*path))
		.collect();
	changed.sort_unstable();

	let mut index = wt.load_index().await?;
	let staged: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();

	// Refuse any changed path whose local state (index or work tree) would be overwritten.
	let mut would_overwrite = Vec::new();
	for &path in &changed {
		let current = staged.get(path);
		if from.get(path) != current || !is_clean(wt, path, current)? {
			would_overwrite.push(path.to_owned());
		}
	}
	if !would_overwrite.is_empty() {
		return Ok(would_overwrite);
	}

	// Apply only the diff; everything else (unrelated staged/dirty entries) is left as-is.
	for &path in &changed {
		match to.get(path) {
			Some((mode, oid)) => write_entry(wt, path, mode, *oid, &mut index).await?,
			None => {
				remove_worktree_file(wt, path)?;
				index.remove(path);
			}
		}
	}
	wt.save_index(&index).await?;
	Ok(Vec::new())
}

/// A recursive tree listing as `path -> (mode, oid)`.
fn tree_map<H: HashAlgorithm>(
	entries: Vec<(String, String, ObjectId<H>)>,
) -> HashMap<String, (String, ObjectId<H>)> {
	entries
		.into_iter()
		.map(|(path, mode, oid)| (path, (mode, oid)))
		.collect()
}

/// Whether `path` is clean enough to overwrite: a tracked path's work-tree file matches the index
/// (`current`), an added path has no untracked file in the way (unless `.gitignore`d), and an absent
/// file is always fine. The index-vs-`HEAD` (staged) check is the caller's. Mirrors
/// [`ensure_no_overwrite`] but as a boolean for the two-way merge's batch check.
fn is_clean<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
	current: Option<&(String, ObjectId<H>)>,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	// An absent file (or one unreachable past a non-directory ancestor) is always fine to overwrite.
	let Some(meta) = wt.work().lstat(path)? else {
		return Ok(true);
	};
	match current {
		Some((mode, oid)) => Ok(matches!(
			blob_of(wt.work(), path, &meta)?,
			Some((woid, wmode)) if woid == *oid && format!("{wmode:o}") == *mode
		)),
		// An untracked file sits where `to` adds a path: refuse unless it is `.gitignore`d.
		None => path_ignored(wt.work(), path),
	}
}

/// Write `path`'s blob into the working tree and record it in the index. Combines a
/// working-tree write with the matching index upsert; used to materialise a whole tree.
pub(crate) async fn write_entry<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
	mode: &str,
	oid: ObjectId<H>,
	index: &mut crate::Index<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	write_worktree_file(wt, path, mode, oid).await?;
	let meta = wt.work().lstat(path)?.ok_or_else(|| {
		std::io::Error::new(
			std::io::ErrorKind::NotFound,
			"just-written entry is missing",
		)
	})?;
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
pub(crate) async fn write_worktree_file<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
	mode: &str,
	oid: ObjectId<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	validate_path(path)?;
	ensure_parents(wt.work(), path)?;
	let content = wt.repository().read_blob(oid).await?;

	if mode == "120000" {
		clear_dest(wt.work(), path)?;
		wt.work().symlink(&content, path)?;
	} else {
		// Replace a directory (a directory->file type change) or a symlink at the destination;
		// a plain file is overwritten in place by the write below.
		match wt.work().lstat(path)? {
			Some(meta) if meta.kind.is_dir() => wt.work().remove_dir_all(path)?,
			Some(meta) if meta.kind.is_symlink() => wt.work().remove_file(path)?,
			_ => {}
		}
		wt.work().write(path, &content, mode == "100755")?;
	}
	Ok(())
}

/// Remove `path` from the working tree (ignoring an already-absent file) and prune any
/// directories left empty above it. Does not touch the index. Guards `path` first — lexically and
/// against symlinked ancestors — so a hostile/corrupt index entry (`../victim`, `.git/…`, or
/// `link/x` through a symlink to outside) cannot delete a file outside the work tree (the checkout
/// CVE class); both `checkout`'s removal loop and `restore` rely on this.
pub(crate) fn remove_worktree_path<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	validate_path(path)?;
	// Don't follow a symlinked ancestor out of the work tree: the file it would reach is not ours,
	// and after a directory→symlink switch the stale child under the old directory is already gone.
	if has_symlinked_ancestor(wt.work(), path) {
		return Ok(());
	}
	let _ = wt.work().remove_file(path);
	remove_empty_parents(wt.work(), path);
	Ok(())
}

/// Like [`remove_worktree_path`], but reports a removal failure. An already-absent file is fine;
/// any other error (e.g. the path is now occupied by a directory) is returned so the caller can
/// refuse rather than silently leave the file in place. Validates `path` first (same escape guard
/// as [`remove_worktree_path`]), for the tree paths the two-way merge removes.
pub(crate) fn remove_worktree_file<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	validate_path(path)?;
	if has_symlinked_ancestor(wt.work(), path) {
		return Ok(());
	}
	match wt.work().remove_file(path) {
		Ok(()) => {}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}
	remove_empty_parents(wt.work(), path);
	Ok(())
}

/// Whether any ancestor directory of `path` is a symlink. A removal must not follow such an ancestor
/// out of the work tree (the checkout CVE class): the file it would reach is not a real tracked file
/// within the tree — and after a directory→symlink switch the stale child under the old directory is
/// already gone — so the removal is skipped. Lexical [`validate_path`] cannot catch this: `link/x`
/// is lexically safe, yet `link` may point outside. A non-existent (or unstattable) ancestor is not
/// a symlink.
fn has_symlinked_ancestor<W: WorkDirFs>(work: &W, path: &str) -> bool {
	let parts: Vec<&str> = path.split('/').collect();
	let mut ancestor = String::new();
	for part in &parts[..parts.len().saturating_sub(1)] {
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(part);
		if work
			.lstat(&ancestor)
			.ok()
			.flatten()
			.is_some_and(|meta| meta.kind.is_symlink())
		{
			return true;
		}
	}
	false
}

fn ensure_no_overwrite<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
	current: Option<&(String, ObjectId<H>)>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	// Absent, or unreachable because a file occupies an ancestor directory (`ENOTDIR`): either way
	// there is nothing at `path` to overwrite.
	let Some(meta) = wt.work().lstat(path)? else {
		return Ok(());
	};
	match current {
		// Tracked: a conflict only if the working file is dirty vs the index.
		Some((mode, oid)) => match blob_of(wt.work(), path, &meta)? {
			Some((woid, wmode)) if woid == *oid && format!("{wmode:o}") == *mode => Ok(()),
			_ => Err(WorktreeError::Conflict(path.to_owned())),
		},
		// Untracked file in the way of a checked-out path — refuse unless it is `.gitignore`d
		// (ignored files are expendable, as git overwrites them).
		None if path_ignored(wt.work(), path)? => Ok(()),
		None => Err(WorktreeError::UntrackedOverwrite(path.to_owned())),
	}
}

/// Whether `path` (a file) is matched by the `.gitignore` rules from the work-tree root down to
/// its parent directory.
fn path_ignored<W: WorkDirFs>(work: &W, path: &str) -> Result<bool, WorktreeError> {
	let stack = ignore_prefix(work, path)?;
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

/// Create the parent directories of `path`, refusing to traverse a symlinked ancestor. A regular
/// file occupying a directory slot is replaced by the directory (a file->directory type change, as
/// git checkout does); a symlink is never traversed or removed here (the checkout CVE class).
fn ensure_parents<W: WorkDirFs>(work: &W, path: &str) -> Result<(), WorktreeError> {
	let parts: Vec<&str> = path.split('/').collect();
	let mut ancestor = String::new();
	for part in &parts[..parts.len().saturating_sub(1)] {
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(part);
		match work.lstat(&ancestor)? {
			Some(meta) if meta.kind.is_dir() => {}
			Some(meta) if meta.kind.is_symlink() => {
				return Err(WorktreeError::UnsafePath(path.to_owned()));
			}
			Some(_) => {
				work.remove_file(&ancestor)?;
				work.create_dir(&ancestor)?;
			}
			None => work.create_dir(&ancestor)?,
		}
	}
	Ok(())
}

/// The first non-ignored untracked path found anywhere under the working-tree directory
/// `dir_rel` — a file (or symlink) whose path is not a tracked index entry and is not matched
/// by `.gitignore`. Replacing the directory with a file would delete it, so a no-force checkout
/// must refuse. `.gitignore`d files (and whole ignored subtrees) are expendable, as in git, so
/// they don't block. `stack` is the ignore stack accumulated from the work-tree root down to
/// `dir_rel`'s parent; this descends `dir_rel`, pushing its own `.gitignore`.
fn first_untracked_under<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	tracked: &HashSet<&str>,
	stack: &mut Vec<DirIgnore>,
) -> Result<Option<String>, WorktreeError> {
	// A wholly-ignored directory is expendable — git doesn't descend into it.
	if ignore::is_ignored(dir_rel, true, stack) {
		return Ok(None);
	}
	let pushed = push_gitignore(work, dir_rel, stack)?;
	let mut found = None;
	for entry in work.read_dir(dir_rel)? {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		let is_dir = entry.kind.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue; // ignored content is expendable
		}
		if is_dir {
			if let Some(hit) = first_untracked_under(work, &rel, tracked, stack)? {
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
fn untracked_file_ancestor<W: WorkDirFs>(
	work: &W,
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
		match work.lstat(&ancestor)? {
			// A file/symlink occupies this ancestor; deeper components cannot exist beyond it.
			Some(meta) if !meta.kind.is_dir() => {
				if tracked.contains(ancestor.as_str()) {
					return Ok(None); // tracked: validated by the removal loop
				}
				let stack = ignore_prefix(work, &ancestor)?;
				let ignored = ignore::is_ignored(&ancestor, false, &stack);
				return Ok((!ignored).then(|| ancestor.clone()));
			}
			Some(_) => {}
			None => return Ok(None),
		}
	}
	Ok(None)
}

/// Build the ignore stack for the ancestors of `dir_rel` — the work-tree root's `.gitignore`
/// and that of each directory strictly above `dir_rel` — ready for matching paths at `dir_rel`.
fn ignore_prefix<W: WorkDirFs>(work: &W, dir_rel: &str) -> Result<Vec<DirIgnore>, WorktreeError> {
	let mut stack = Vec::new();
	push_gitignore(work, "", &mut stack)?;
	let components: Vec<&str> = dir_rel.split('/').filter(|part| !part.is_empty()).collect();
	let mut ancestor = String::new();
	for component in &components[..components.len().saturating_sub(1)] {
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(component);
		push_gitignore(work, &ancestor, &mut stack)?;
	}
	Ok(stack)
}

/// Remove whatever currently occupies `path` — a file, a symlink, or a whole directory —
/// leaving nothing behind, so a new entry can be written in its place.
fn clear_dest<W: WorkDirFs>(work: &W, path: &str) -> Result<(), WorktreeError> {
	match work.lstat(path)? {
		Some(meta) if meta.kind.is_dir() => work.remove_dir_all(path)?,
		Some(_) => work.remove_file(path)?,
		None => {}
	}
	Ok(())
}

/// Prune directories left empty above `path`, from its parent upward, stopping at the first that is
/// non-empty (or the work-tree root).
fn remove_empty_parents<W: WorkDirFs>(work: &W, path: &str) {
	let parts: Vec<&str> = path.split('/').collect();
	for depth in (1..parts.len()).rev() {
		let dir = parts[..depth].join("/");
		if work.remove_dir(&dir).is_err() {
			break;
		}
	}
}
