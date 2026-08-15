//! Three-way status: HEAD tree vs index (staged) and index vs working tree
//! (unstaged), plus untracked files. Untracked detection applies `.gitignore` and
//! collapses fully-untracked directories the way git's default mode does.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};

use crate::excludes::StandardExcludes;
use crate::fsmeta::{blob_of, effective_mode, join_rel, push_gitignore};
use crate::ignore::{self, DirIgnore};
use crate::submodule_head_oid;
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
	excludes_file: Option<&str>,
) -> Result<Status, WorktreeError> {
	// `core.fileMode` (git's `trust_executable_bit`): when `false`, an executable-bit-only difference between
	// the working tree and the index is *not* a modification. Resolved with git's worktree precedence — a
	// per-worktree override in `config.worktree` (when `extensions.worktreeConfig` is set) wins over the common
	// `config`; unset defaults to `true` (honour the bit). Getting this wrong toward "clean" would let removal
	// delete a genuinely-modified checkout, so the default and the override both fail safe toward `true`.
	let file_mode = worktree_file_mode(wt).await;
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

	// git's standard excludes for untracked detection: the `core.ignoreCase` fold flag plus the
	// whole-tree exclude levels (`core.excludesFile`, `.git/info/exclude`) that sit below per-directory
	// `.gitignore`. Seeding these makes `status` agree with `ls-files -o` and stock git, where it
	// previously read only `.gitignore` and matched case-sensitively.
	let StandardExcludes { fold, base } =
		crate::excludes::standard_excludes(wt, excludes_file).await?;
	// A conflicted path is tracked (it has a working-tree file), so exclude it from untracked. Under
	// `core.ignoreCase` git matches a working-tree entry to a tracked index path case-folded (a disk
	// `FOO` counts as the tracked `foo`), so fold the membership keys the same way the lookups below do.
	let tracked: HashSet<String> = index_map
		.keys()
		.cloned()
		.chain(unmerged.keys().cloned())
		.map(|path| fold_key(&path, fold))
		.collect();
	// Tracked submodule (gitlink) paths, folded — a gitlink's on-disk directory is git's to track, never
	// listed untracked (unlike a tracked file replaced by an untracked directory).
	let gitlinks: HashSet<String> = index_map
		.iter()
		.filter(|(_, (mode, _))| mode == "160000")
		.map(|(path, _)| fold_key(path, fold))
		.collect();
	let mut untracked = Vec::new();
	let mut ignore_stack: Vec<DirIgnore> = base;
	collect_untracked(
		wt.work(),
		"",
		&tracked,
		&gitlinks,
		&mut ignore_stack,
		&mut untracked,
		fold,
	)?;

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

	// Working tree vs index (the Y column). A **skip-worktree** (sparse) entry is compared only when its
	// file is PRESENT: git ignores the working tree for an omitted (absent) sparse path — so its absence is
	// not a deletion — but a file the user recreated or edited at that path is reported modified (git clears
	// the bit when it notices the file; gitana reports it without mutating the index).
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		if entry.skip_worktree && wt.work().lstat(&entry.path)?.is_none() {
			continue;
		}
		let code = if entry.mode == 0o160000 {
			// A submodule (gitlink) is modified iff its checked-out `HEAD` differs from the recorded commit;
			// git ignores the submodule's own dirty working content by default. An unresolvable submodule
			// (an unhandled `.git` layout) is treated as unchanged rather than a false `M` (as `ls-files -m`).
			match submodule_head_oid(wt, &entry.path).await {
				Some(head) if head != entry.oid => 'M',
				_ => ' ',
			}
		} else {
			worktree_change(wt.work(), entry, &entry.path, file_mode)?
		};
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

/// The stage-0 tracked paths **present on disk whose content or mode diverges from the index**, verified by
/// **always hashing the working file** — never the `stat_matches` fast path that [`compute`]/`status()` (and
/// git) take. This is a **removal-only** re-verification: recursive removal must not delete a checkout on the
/// strength of a stat-cache "clean", because that cache can hide a real edit — a same-size rewrite that
/// preserves the cached stat fields, or any edit within a coarse-timestamp filesystem's granularity
/// (FAT/exFAT) — and it also omits skip-worktree entries entirely (git ignores the working tree for them, so
/// a `update-index --skip-worktree`d-then-edited file shows nothing in `status`). Hashing every present
/// tracked file closes both holes.
///
/// An **absent** entry is never reported: a deleted non-sparse file is reliably caught by `status` (`lstat` →
/// `D`, no stat cache involved), and an absent skip-worktree path is the ordinary sparse-checkout case. A
/// present entry that hashes equal (and whose mode matches under the resolved `core.fileMode`) is
/// reconstructable from the object store, so it too is omitted; only a genuinely diverged present file — the
/// content at risk of loss — is returned.
pub(crate) async fn diverged_tracked_content<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Vec<String>, WorktreeError> {
	let file_mode = worktree_file_mode(wt).await;
	let index = wt.load_index().await?;
	let mut out = Vec::new();
	for entry in index.entries.iter().filter(|e| e.stage == 0) {
		if matches!(
			worktree_content_state(wt, entry, file_mode).await?,
			WorktreeContent::Diverged
		) {
			out.push(entry.path.clone());
		}
	}
	Ok(out)
}

/// The working-tree state of a tracked path relative to its index blob, established by **hashing** the
/// present file rather than trusting the stat cache — the removal-safe classification shared by
/// [`diverged_tracked_content`] and sparse reapply. Never uses the `stat_matches` fast path, which can
/// hide a same-size / coarse-timestamp edit.
pub(crate) enum WorktreeContent {
	/// No file at the path — a deletion (`status` catches it) or an already-omitted sparse path.
	Absent,
	/// A present file that hashes back to the index blob (mode included) and is reconstructable from a
	/// verified object-store copy — safe to remove or leave.
	Reconstructable,
	/// A present file whose bytes or mode differ from the index blob (or whose stored blob is
	/// missing/corrupt) — content at risk that must not be overwritten or deleted.
	Diverged,
}

/// Classify `entry`'s working-tree file (see [`WorktreeContent`]). Absent → `Absent`; present and equal
/// to a verified stored blob → `Reconstructable`; otherwise → `Diverged`. Always hashes the present
/// file, so a stat-preserving or coarse-timestamp edit is caught.
pub(crate) async fn worktree_content_state<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	entry: &IndexEntry<H>,
	file_mode: bool,
) -> Result<WorktreeContent, WorktreeError> {
	let Some(meta) = wt.work().lstat(&entry.path)? else {
		return Ok(WorktreeContent::Absent);
	};
	// Deliberately *no* `stat_matches` shortcut: hash the working file and compare oid + mode directly.
	let diverged = match blob_of::<W, H>(wt.work(), &entry.path, &meta)? {
		Some((oid, _))
			if oid == entry.oid
				&& modes_equivalent(effective_mode(&meta, entry.mode), entry.mode, file_mode) =>
		{
			// The working file matches the index — but it is only "reconstructable" (safe to delete) if the
			// object store holds a *valid* copy. Existence alone is not enough: a present-but-corrupt loose
			// object, or a pack naming an unreadable object, would leave the working file as the sole valid
			// copy. Read the stored blob and confirm it hashes back to the indexed oid; a read failure or a
			// hash mismatch means the checkout is not safe to delete, so treat it as diverged (preserve).
			match wt.repository().read_blob(entry.oid).await {
				Ok(bytes) => ObjectId::<H>::compute(ObjectKind::Blob, &bytes) != entry.oid,
				Err(_) => true,
			}
		}
		// Content or mode differs, or neither a regular file nor a symlink now sits at a tracked path (e.g. a
		// directory replaced it) — a divergence from the tracked blob; not safe to delete blindly.
		_ => true,
	};
	Ok(if diverged {
		WorktreeContent::Diverged
	} else {
		WorktreeContent::Reconstructable
	})
}

/// Whether the index carries any change relative to `HEAD` — a **staged** add/modify/delete, or an unmerged
/// (conflicted) entry — computed from the index and the `HEAD` tree **without touching the working tree**. It
/// is therefore valid even when the checkout is gone, the removal-safety check a checkout-missing partial
/// needs: cleaning such a partial drops the admin dir (and its index), which would erase staged state and
/// leave index-only blobs unreferenced (gc-able). An unborn `HEAD` (no commit) with a non-empty index counts
/// as staged additions.
///
/// A **genuinely absent** index (no `index` file — e.g. `create` interrupted after `HEAD` was published but
/// before the checkout materialised its index) is *not* staged work: there is nothing to lose, so this returns
/// `false` (a recoverable partial). Only a *present* index that differs from `HEAD` is staged content — an
/// absent index must not be conflated with a deliberately-empty one (which vs a non-empty `HEAD` would be a
/// staged deletion of every tracked path).
pub(crate) async fn has_staged_changes<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<bool, WorktreeError> {
	// No index file at all → a recoverable partial with no staged state, not an all-paths staged deletion.
	if !wt
		.repository()
		.objects()
		.file_store()
		.exists("index")
		.await?
	{
		return Ok(false);
	}
	let index = wt.load_index().await?;
	// Any unmerged (stage > 0) entry is conflicted work.
	if index.entries.iter().any(|e| e.stage != 0) {
		return Ok(true);
	}
	// Stage-0 index vs the HEAD tree: an added / removed / modified path is a staged change. Built the same way
	// `compute`'s X column is, so the two agree.
	let index_map: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();
	Ok(index_map != head_entries(wt).await?)
}

/// The effective `core.fileMode` for this worktree, honouring git's worktree-config precedence: a
/// per-worktree `config.worktree` override (only consulted when the common config sets
/// `extensions.worktreeConfig`) wins over the common `config`; an unset value defaults to `true`.
///
/// This gates whether an exec-bit change is a *modification*, so resolving it wrongly toward `false` would
/// let removal delete a checkout git considers modified. It therefore **fails closed to `true`** (honour the
/// exec bit → refuse) whenever `false` cannot be established *simply and certainly*: an unreadable common
/// config; an `include`/`includeIf` in the local config (which we do not process and which could override
/// `core.fileMode`); an unparseable value; or a `config.worktree` that is present but unreadable / non-UTF-8
/// / malformed. Only a genuinely-absent `config.worktree` (`NotFound`) falls back to the common value.
pub(crate) async fn worktree_file_mode<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> bool {
	let Ok(common) = wt.repository().read_config().await else {
		return true; // unreadable common config — cannot establish `false`, so honour the exec bit
	};
	// We do not process `include`/`includeIf`; either could set `core.fileMode` after the local value, so a
	// local value cannot be trusted when includes are present — fail closed.
	if config_has_includes(&common) {
		return true;
	}
	let common_mode = common.get_bool("core", None, "filemode");
	// `extensions.worktreeConfig` gates whether a per-worktree override is consulted. An **invalid** value is a
	// config git errors on, so fail closed immediately rather than consult the override or fall back.
	let worktree_config_enabled = match common.get_bool("extensions", None, "worktreeconfig") {
		Ok(Some(enabled)) => enabled,
		Ok(None) => false,
		Err(_) => return true,
	};
	if worktree_config_enabled {
		match wt
			.repository()
			.objects()
			.file_store()
			.read_path("config.worktree")
			.await
		{
			Ok(bytes) => {
				// Present but non-UTF-8 / malformed / with an unparseable value → fail closed (git errors here).
				let Ok(text) = String::from_utf8(bytes) else {
					return true;
				};
				let Ok(over) = gitana_config::GitConfig::parse(&text) else {
					return true;
				};
				// The override file may itself include another that overrides `core.fileMode` — fail closed.
				if config_has_includes(&over) {
					return true;
				}
				match over.get_bool("core", None, "filemode") {
					Ok(Some(mode)) => return mode, // explicit, parseable override wins
					Err(_) => return true,         // present but unparseable → fail closed
					Ok(None) => {}                 // no override key → fall through to the common value
				}
			}
			Err(FileStoreError::NotFound) => {} // no override file — use the common value
			Err(_) => return true,              // present but unreadable — fail closed
		}
	}
	// The common value; absent (git's default) or unparseable both honour the exec bit.
	common_mode.ok().flatten().unwrap_or(true)
}

/// Whether a config carries an `include` or `includeIf` directive. gitana does not process these, so any
/// config value they might override cannot be trusted for a safety decision. A directive is detected even when
/// *valueless* (`get_all_raw` keeps a bare `include.path`, which `get_all` drops) — git errors on that too.
pub(crate) fn config_has_includes(config: &gitana_config::GitConfig) -> bool {
	!config.get_all_raw("include", None, "path").is_empty()
		|| !config.subsections("includeIf").is_empty()
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
	gitlinks: &HashSet<String>,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
	fold: bool,
) -> Result<(), WorktreeError> {
	let pushed = push_gitignore(work, dir_rel, stack)?;

	for entry in work.read_dir(dir_rel)? {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		// The entry's kind is an `lstat` (a symlinked directory is a symlink, not a directory).
		let is_dir = entry.kind.is_dir();
		if ignore::is_ignored_fold(&rel, is_dir, stack, fold) {
			continue;
		}
		// A directory that is itself a tracked SUBMODULE (a gitlink) is git's to track — never listed
		// untracked, and not descended into (its contents belong to the submodule). Its status is the
		// tracked Y-column comparison above. (A tracked *file* replaced by a directory is NOT a gitlink and
		// falls through to the normal untracked handling.)
		if is_dir && gitlinks.contains(&fold_key(&rel, fold)) {
			continue;
		}
		// Membership is checked case-folded under `core.ignoreCase` (the `tracked` keys are already
		// folded); the emitted path is always the real on-disk spelling.
		if is_dir {
			let prefix = format!("{rel}/");
			let prefix_key = fold_key(&prefix, fold);
			if tracked.iter().any(|path| path.starts_with(&prefix_key)) {
				collect_untracked(work, &rel, tracked, gitlinks, stack, out, fold)?;
			} else if dir_has_unignored(work, &rel, stack, fold)? {
				// A fully-untracked directory collapses to a single `dir/` — but only when it holds some
				// non-ignored content. git omits a directory whose entire content is ignored (by any
				// standard exclude source), so descend to check before collapsing.
				out.push(prefix);
			}
		} else if !tracked.contains(&fold_key(&rel, fold)) {
			out.push(rel);
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}

/// Whether the untracked directory `dir_rel` holds at least one non-ignored entry (recursively). git
/// collapses an untracked directory to `dir/` only when it has some non-ignored content; one whose
/// entire content is ignored (by any standard exclude source) is omitted from status. `stack` is the
/// ignore stack down to `dir_rel`'s parent; this pushes `dir_rel`'s own `.gitignore` while descending.
fn dir_has_unignored<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
	fold: bool,
) -> Result<bool, WorktreeError> {
	// An untracked embedded git repository is reportable regardless of its content — git lists the single
	// `?? dir/` and never descends (probed vs git 2.55). Omitting it would let a default `worktree remove`
	// recursively delete the nested repo. Recognise a valid `.git` *directory* repo, and conservatively any
	// `.git` *file* (a gitfile — a linked worktree or submodule): over-reporting a bogus gitfile is the
	// safe direction (it refuses removal, never deletes).
	if crate::ls_files::is_embedded_repo(work, dir_rel)
		|| matches!(
			work.lstat(&format!("{dir_rel}/.git")),
			Ok(Some(meta)) if !meta.kind.is_dir()
		) {
		return Ok(true);
	}
	// An unreadable directory is warned-and-omitted by git (not fatal, exit 0), so treat it as having no
	// reportable content rather than aborting the whole status. Read the entries before pushing this
	// directory's `.gitignore` so a permission error here never propagates.
	let Ok(entries) = work.read_dir(dir_rel) else {
		return Ok(false);
	};
	// An unusable per-directory `.gitignore` (a directory at that path, or permission-denied) contributes
	// no rules and must not abort status — git tolerates it and reports the parent as `?? dir/`.
	let pushed = push_gitignore(work, dir_rel, stack).unwrap_or(false);
	let mut found = false;
	for entry in entries {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		let is_dir = entry.kind.is_dir();
		if ignore::is_ignored_fold(&rel, is_dir, stack, fold) {
			continue;
		}
		if is_dir {
			if dir_has_unignored(work, &rel, stack, fold)? {
				found = true;
				break;
			}
		} else {
			found = true;
			break;
		}
	}
	if pushed {
		stack.pop();
	}
	Ok(found)
}

/// The membership key for `path` under `core.ignoreCase`: ASCII-lower-cased when `fold`, else `path`
/// unchanged. Used to make tracked-vs-untracked detection case-insensitive without altering the path
/// git reports.
fn fold_key(path: &str, fold: bool) -> String {
	if fold {
		path.to_ascii_lowercase()
	} else {
		path.to_owned()
	}
}

pub(crate) fn worktree_change<W: WorkDirFs, H: HashAlgorithm>(
	work: &W,
	entry: &IndexEntry<H>,
	path: &str,
	file_mode: bool,
) -> Result<char, WorktreeError> {
	let Some(meta) = work.lstat(path)? else {
		return Ok('D');
	};
	if stat_matches(entry, &meta) {
		return Ok(' ');
	}
	match blob_of(work, path, &meta)? {
		Some((oid, _))
			if oid == entry.oid
				&& modes_equivalent(effective_mode(&meta, entry.mode), entry.mode, file_mode) =>
		{
			Ok(' ')
		}
		_ => Ok('M'),
	}
}

/// Whether a working-tree mode matches the indexed mode. With `file_mode` (`core.fileMode=true`) the modes
/// must be identical; with `core.fileMode=false` an executable-bit-only difference is ignored — a regular
/// file `100644` and `100755` are equivalent — while symlink (`120000`) and gitlink (`160000`) types still
/// differ from a regular file.
fn modes_equivalent(actual: u32, expected: u32, file_mode: bool) -> bool {
	if file_mode {
		actual == expected
	} else {
		regular_mode_class(actual) == regular_mode_class(expected)
	}
}

/// Collapse a regular file's executable bit so `100644`/`100755` share a class; other git object types
/// (symlink, gitlink, tree) keep their own type bits.
fn regular_mode_class(mode: u32) -> u32 {
	if mode & 0o170000 == 0o100000 {
		0o100644
	} else {
		mode
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

	/// The removal-safety re-verification must hash the working file rather than trust the index stat cache: a
	/// crafted entry whose cached stat exactly matches the on-disk file but whose oid does *not* match its
	/// content is the stat-cache hole (a same-size/stat-preserving rewrite, or a coarse-timestamp filesystem).
	/// `worktree_change` — the fast path `status()` and git take — reports it clean; `diverged_tracked_content`
	/// must still catch it.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn diverged_tracked_content_hashes_past_a_matching_stat_cache() {
		use std::sync::atomic::{AtomicU32, Ordering};

		use cap_std::ambient_authority;
		use cap_std::fs::Dir;
		use gitana_file_store_local::{CapWorkDir, LocalFileStore};
		use gitana_object_store::ObjectStore;
		use gitana_repository::Repository;

		use crate::IndexEntry;
		use crate::fsmeta::{mode_of, stat_of};

		static SEQ: AtomicU32 = AtomicU32::new(0);
		let root = std::env::temp_dir().join(format!(
			"gitana-diverged-{}-{}",
			std::process::id(),
			SEQ.fetch_add(1, Ordering::Relaxed)
		));
		let git_dir = root.join(".git");
		std::fs::create_dir_all(&git_dir).unwrap();
		// Same length before and after, so a stat cache that records the pre-edit size still "matches".
		std::fs::write(root.join("a.txt"), b"AAAA").unwrap();

		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap(),
		)));
		let wt = WorkTree::new(
			repo,
			CapWorkDir::from_dir(Dir::open_ambient_dir(&root, ambient_authority()).unwrap()),
			&git_dir,
		);

		// Build an entry whose stat cache exactly matches the on-disk file, but whose oid is for *different*
		// content — the state a stat-preserving rewrite leaves behind.
		let meta = wt.work().lstat("a.txt").unwrap().expect("a.txt exists");
		let entry = IndexEntry::<Sha256> {
			stat: stat_of(&meta),
			mode: mode_of(&meta),
			oid: ObjectId::<Sha256>::compute(ObjectKind::Blob, b"BBBB"),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "a.txt".to_owned(),
		};

		// The fast path is fooled — stat matches, so it reports the file unchanged.
		assert_eq!(
			worktree_change(wt.work(), &entry, "a.txt", true).unwrap(),
			' ',
			"the stat-cache fast path reports the diverged file as clean (the hole)"
		);

		// The removal re-verification hashes the file and catches the divergence.
		let mut index = Index::new();
		index.entries.push(entry);
		wt.save_index(&index).await.unwrap();
		let diverged = diverged_tracked_content(&wt).await.unwrap();
		assert_eq!(
			diverged,
			vec!["a.txt".to_owned()],
			"content re-verification must hash past the stat cache"
		);

		let _ = std::fs::remove_dir_all(&root);
	}

	/// A working file that hashes to its index oid is only reconstructable if that object is actually stored —
	/// with the object store empty, the working file is the sole copy and must be flagged (not treated as safe
	/// to delete).
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn diverged_tracked_content_flags_a_file_whose_object_is_missing() {
		use std::sync::atomic::{AtomicU32, Ordering};

		use cap_std::ambient_authority;
		use cap_std::fs::Dir;
		use gitana_file_store_local::{CapWorkDir, LocalFileStore};
		use gitana_object_store::ObjectStore;
		use gitana_repository::Repository;

		use crate::IndexEntry;
		use crate::fsmeta::{mode_of, stat_of};

		static SEQ: AtomicU32 = AtomicU32::new(0);
		let root = std::env::temp_dir().join(format!(
			"gitana-missingobj-{}-{}",
			std::process::id(),
			SEQ.fetch_add(1, Ordering::Relaxed)
		));
		let git_dir = root.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::write(root.join("a.txt"), b"AAAA").unwrap();

		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap(),
		)));
		let wt = WorkTree::new(
			repo,
			CapWorkDir::from_dir(Dir::open_ambient_dir(&root, ambient_authority()).unwrap()),
			&git_dir,
		);

		// The entry's oid *matches* the working file's content — but nothing was written to the object store.
		let meta = wt.work().lstat("a.txt").unwrap().expect("a.txt exists");
		let entry = IndexEntry::<Sha256> {
			stat: stat_of(&meta),
			mode: mode_of(&meta),
			oid: ObjectId::<Sha256>::compute(ObjectKind::Blob, b"AAAA"),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "a.txt".to_owned(),
		};
		let mut index = Index::new();
		index.entries.push(entry);
		wt.save_index(&index).await.unwrap();

		let diverged = diverged_tracked_content(&wt).await.unwrap();
		assert_eq!(
			diverged,
			vec!["a.txt".to_owned()],
			"a hash-matching file whose object is missing is the sole copy — must be flagged, not deletable"
		);

		let _ = std::fs::remove_dir_all(&root);
	}
}
