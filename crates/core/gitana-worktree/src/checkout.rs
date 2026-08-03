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

use crate::fsmeta::{blob_of, effective_mode, join_rel, push_gitignore, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{IndexEntry, SparseCheckout, WorkTree, WorktreeError};

/// A borrowed stage-0 index entry: its `(octal-mode, oid)` pair.
type EntryRef<'a, H> = &'a (String, ObjectId<H>);
/// Current stage-0 entries grouped by fold-key, each keeping its actual index spelling — a case-colliding
/// index (`Foo`+`foo`) keeps both under one key.
type FoldGroups<'a, H> = std::collections::HashMap<String, Vec<(&'a str, EntryRef<'a, H>)>>;
/// The (path, entry) pairs whose working-tree cleanliness the checkout guard verifies before writing a
/// target path; `None` entry = a new addition checked only for an in-the-way untracked file.
type CheckList<'a, H> = Vec<(&'a str, Option<EntryRef<'a, H>>)>;

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	tree: ObjectId<H>,
	force: bool,
	excludes_file: Option<&str>,
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

	let sparse = wt.sparse_checkout().await?;
	let mut index = wt.load_index().await?;
	let current: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();

	// `core.ignoreCase`: git's index is case-insensitive under it, so an index entry and a target entry
	// whose paths fold-equal are the *same* path (a `Foo`→`foo` case-rename across branches). This governs
	// the whole diff — the stray-removal below runs even under `force`, and mis-classifying a case-rename
	// as an add+delete would delete the re-created file — so resolve the fold flag unconditionally, not
	// just for the non-force guard. (Reads/validates `core.ignoreCase`, which git does on every checkout,
	// forced or not.)
	let fold = crate::excludes::ignore_case(wt).await?;
	// Target paths keyed by fold — the identity of what the checkout will (re)create.
	let target_fold: HashSet<String> = target_paths.keys().map(|p| fold_key(p, fold)).collect();
	// Current stage-0 entries indexed by fold, for the guard's case-insensitive lookups. The value keeps
	// the entry's ACTUAL index spelling alongside its (mode, oid): a case-rename must check the file that
	// really exists (`Foo`), not the target spelling (`foo`) which does not exist on a case-*sensitive*
	// filesystem — otherwise the cleanliness `lstat` sees nothing and a dirty edit is silently discarded.
	// Every current stage-0 entry grouped by fold-key. A case-colliding index (`Foo` and `foo`) keeps BOTH
	// entries per key, so the guard below can check the shared inode against each blob rather than one
	// arbitrarily-kept entry (which made a colliding-index checkout nondeterministic).
	let current_fold_all: FoldGroups<H> = {
		let mut m: FoldGroups<H> = HashMap::new();
		for (k, v) in &current {
			m.entry(fold_key(k, fold))
				.or_default()
				.push((k.as_str(), v));
		}
		m
	};

	// Under `core.ignoreCase`, a target entry and a current index entry whose paths fold-equal are the same
	// path — but that equivalence must not silently overwrite a LOCALLY STAGED case-rename. If the index holds
	// `foo` (a staged rename of HEAD's `Foo`) and the target keeps `Foo`, git carries the staged `foo` forward
	// (probed vs git 2.55: `D Foo` / `A foo`), it does not rewrite it back to `Foo`. Distinguish the two by
	// HEAD: a current spelling HEAD also tracks is a genuine BRANCH rename (apply the target's spelling); one
	// absent from HEAD is a STAGED recase to PRESERVE (keep the index entry, do not materialise the target's
	// other-cased spelling). `head_paths` is read once, only when folding. `force` skips this: it discards
	// local changes, staged recase included, resetting the working tree to the target (git's `-f`).
	let head_entries = if fold && !force {
		head_tree_entries(wt).await?
	} else {
		Vec::new()
	};
	let head_paths: HashSet<&str> = head_entries.iter().map(|(p, _, _)| p.as_str()).collect();
	// HEAD's entry per fold-key: to tell a target that KEEPS a staged-renamed path unchanged from HEAD (the
	// staged recase is preserved) from one that MODIFIES it (refuse — the incoming edit conflicts with the
	// staged rename, as git does).
	let head_fold: HashMap<String, (&str, &ObjectId<H>)> = head_entries
		.iter()
		.map(|(p, m, o)| (fold_key(p, fold), (m.as_str(), o)))
		.collect();
	// A locally staged case-rename splits into two outcomes, decided against HEAD:
	//  * PRESERVE — the target keeps this fold-key UNCHANGED from HEAD (same blob/mode): git carries the staged
	//    recase forward, so keep the index entry and do not materialise the target's other-cased spelling.
	//  * REFUSE — the target MODIFIES this fold-key (its entry differs from HEAD): the incoming edit conflicts
	//    with the staged rename, and git aborts the switch (probed vs git 2.55).
	let mut preserve_folds: HashSet<String> = HashSet::new();
	let mut refuse_folds: HashSet<String> = HashSet::new();
	if fold && !force {
		// Index the HEAD and target spellings by fold-key ONCE, so the per-entry classification below stays
		// linear rather than rescanning HEAD and the target for every staged rename (a bulk recase would
		// otherwise be O(N²)).
		//   * `head_spelling_indexed` — fold-keys whose HEAD spelling is still in the index (a colliding
		//     addition, not a clean rename);
		//   * `target_spelling_indexed` — fold-keys whose target spelling is a retained current entry (a case
		//     collision);
		//   * `target_fold_entry` — the target's `(mode, oid)` per fold-key, to compare against HEAD's.
		let head_spelling_indexed: HashSet<String> = head_entries
			.iter()
			.filter(|(hp, _, _)| current.contains_key(hp.as_str()))
			.map(|(hp, _, _)| fold_key(hp, fold))
			.collect();
		let target_spelling_indexed: HashSet<String> = target_paths
			.iter()
			.filter(|(t, _)| current.contains_key(**t))
			.map(|(t, _)| fold_key(t, fold))
			.collect();
		let target_fold_entry: HashMap<String, (&str, ObjectId<H>)> = target_paths
			.iter()
			.map(|(t, (m, o))| (fold_key(t, fold), (*m, *o)))
			.collect();
		for path in current.keys() {
			// The target keeps this exact spelling → not a rename at all.
			if target_paths.contains_key(path.as_str()) {
				continue;
			}
			let key = fold_key(path, fold);
			// The target does not recreate this fold-key → a true removal, not a rename.
			if !target_fold.contains(&key) {
				continue;
			}
			// HEAD tracks this exact spelling → a genuine branch case-rename, applied as before.
			if head_paths.contains(path.as_str()) {
				continue;
			}
			// HEAD's OWN spelling for this fold-key is still in the index → a colliding ADDITION (the index keeps
			// `Foo` and *additionally* stages `foo`), not a clean rename. Leave it to the normal rename/colliding
			// handling below (the target's recase applies; a dirty collision refuses).
			if head_spelling_indexed.contains(&key) {
				continue;
			}
			// The target's own spelling for this fold-key is a retained current entry → a case *collision*
			// (`Foo` kept, `foo` beside it), handled by the collision bucket below, not a preserve.
			if target_spelling_indexed.contains(&key) {
				continue;
			}
			// Preserve only when the target keeps this fold-key exactly as HEAD has it; a modified target entry
			// (or one HEAD lacks entirely) conflicts with the staged rename and must refuse.
			let target_eq_head = match (target_fold_entry.get(&key), head_fold.get(&key)) {
				(Some((tm, to)), Some((hm, ho))) => tm == hm && &to == ho,
				_ => false,
			};
			if target_eq_head {
				preserve_folds.insert(key);
			} else {
				refuse_folds.insert(key);
			}
		}
	}

	// git's whole-tree standard excludes for the overwrite guard's "is this in-the-way file expendable?"
	// test (`core.excludesFile`, `.git/info/exclude`) below per-directory `.gitignore`. Loaded on *every*
	// checkout — a directory at either path is fatal to git even under `--force` (probed vs git 2.55:
	// `checkout -f`/`switch -f` still abort) — because `force` skips the overwrite *protection*, not
	// config validation. The patterns are only *applied* by the non-force guard below.
	let base = crate::excludes::load_base(wt, excludes_file).await?;

	if !force {
		// Previously this guard read only `.gitignore` case-sensitively, so a file ignored solely by a
		// global/`info/exclude` rule — or a case-variant of a `.gitignore` rule — wrongly blocked a
		// checkout git would have allowed.
		// Tracked paths for the untracked-overwrite guard, matched by **exact** spelling (not folded). On a
		// case-sensitive filesystem a folded match does not prove a disk entry is the tracked file — e.g. an
		// untracked `dir/x` beside a tracked `Dir/x` — and waiving the guard would delete it. Exact
		// membership only ever *over*-refuses (treats a case-variant tracked file as untracked), the safe
		// direction for a data-destroying guard.
		let tracked: HashSet<&str> = current.keys().map(String::as_str).collect();
		for (path, (mode, oid)) in &target_paths {
			let key = fold_key(path, fold);
			// A staged case-rename this checkout preserves is not materialised, so it cannot overwrite anything —
			// skip the overwrite guard for the target's other-cased spelling (see `preserve_folds`).
			if preserve_folds.contains(&key) {
				continue;
			}
			// A staged case-rename the DESTINATION modifies (its entry differs from HEAD) conflicts with the
			// staged rename: git aborts. Refuse before writing, naming the target path.
			if refuse_folds.contains(&key) {
				return Err(WorktreeError::Conflict((*path).to_string()));
			}
			// A path needs materialising when its **exact** entry differs — so a case-only rename
			// (`Foo`→`foo`, identical blob/mode) still counts as differing and gets the cleanliness check
			// below, matching git, which refuses to overwrite a *dirty* case-rename (probed vs git 2.55).
			let differs = current
				.get(*path)
				.is_none_or(|(cm, co)| cm != mode || co != oid);
			// The working-tree files whose cleanliness we must check before (re)writing this target path, each
			// against the blob it is tracked under. Prefer the target's EXACT current entry; otherwise EVERY
			// case-colliding entry that folds to this key (`Foo` and `foo`), so the shared inode is verified
			// against each blob and a dirty state relative to ANY of them refuses — not one arbitrarily-kept
			// entry, which made a colliding-index checkout (including a recase to a third spelling) nondeterministic.
			// With no current entry at all, the target path itself (a new addition, checked for an in-the-way
			// untracked file).
			let checks: CheckList<H> = if let Some(entry) = current.get(*path) {
				vec![(*path, Some(entry))]
			} else {
				match current_fold_all.get(&key) {
					Some(entries) if !entries.is_empty() => {
						entries.iter().map(|(cp, ce)| (*cp, Some(*ce))).collect()
					}
					_ => vec![(*path, None)],
				}
			};
			// An excluded target path is not materialised. A *new* excluded addition cannot overwrite an
			// in-the-way untracked file — git completes the checkout and leaves that file visible — so skip
			// the preflight for it. But an excluded path that is a *prior tracked entry* being CHANGED still
			// needs the cleanliness check: git refuses the checkout when its present working-tree file is
			// locally modified, since re-omitting the path would discard that edit (probed vs git 2.50.1 —
			// a dirty recreated omitted file refuses; a clean one is fine). Retaining the check here also
			// stops a dirty checkout from silently absorbing an edit that happens to equal the target.
			if sparse
				.as_ref()
				.is_some_and(|matcher| !matcher.includes(path))
			{
				if differs {
					for (cp, ce) in &checks {
						if ce.is_some() {
							ensure_no_overwrite(wt, cp, *ce, &base, fold)?;
						}
					}
				}
				continue;
			}
			if !differs {
				continue;
			}
			match wt.work().lstat(path)? {
				// A directory occupies this file's slot (a directory->file change). Replacing it
				// would delete everything under it, so refuse if it holds a non-ignored untracked
				// file; its tracked contents are validated for cleanliness by the removal loop.
				Some(meta) if meta.kind.is_dir() => {
					let mut stack = ignore_prefix(wt.work(), path, &base)?;
					if let Some(untracked) =
						first_untracked_under(wt.work(), path, &tracked, &mut stack, fold)?
					{
						return Err(WorktreeError::UntrackedOverwrite(untracked));
					}
				}
				_ => {
					for (cp, ce) in &checks {
						ensure_no_overwrite(wt, cp, *ce, &base, fold)?;
					}
					// On a case-*sensitive* filesystem a case-rename's target spelling can be a *distinct*
					// file beside the tracked one (an untracked or recased `foo` next to `Foo`); the write
					// would clobber it. Verify that spelling too (against the tracked blob, so on a
					// case-*insensitive* filesystem — where it is the same inode — a clean rename still
					// passes). Only when it is not already among the checks above.
					if !checks.iter().any(|(cp, _)| cp == path) {
						let entry = checks.first().and_then(|(_, ce)| *ce);
						ensure_no_overwrite(wt, path, entry, &base, fold)?;
					}
				}
			}
			// A file->directory change removes a file occupying an ancestor slot; refuse if that
			// file is an untracked, non-ignored file (a tracked ancestor is validated by the
			// removal loop, an ignored one is expendable).
			if let Some(untracked) = untracked_file_ancestor(wt.work(), path, &tracked, &base, fold)? {
				return Err(WorktreeError::UntrackedOverwrite(untracked));
			}
		}
		for (path, entry) in &current {
			// A current entry whose fold-key the target does not recreate is a true removal (a case-rename's
			// key IS in `target_fold`, so it is handled by the classification below, not here). Validate the
			// working file against THIS entry's OWN blob — so a case-colliding index (`Foo`+`foo`, different
			// blobs) checks each colliding entry separately and refuses when the shared working file is dirty
			// relative to *any* of them, exactly as git does (probed vs git 2.55: switching a colliding pair to
			// a target lacking the fold-key refuses, naming every entry the file diverges from). Consulting the
			// arbitrarily-kept folded entry instead made that verdict nondeterministic under `HashMap` ordering.
			if !target_fold.contains(&fold_key(path, fold)) {
				ensure_no_overwrite(wt, path, Some(entry), &base, fold)?;
			}
		}
	}

	// Take the index lock before touching the working tree, so a held lock aborts here — before any
	// filesystem change — rather than after, which would leave the tree inconsistent with the index.
	// On a mid-materialise failure the lock is released (not orphaned) and the index is left unwritten,
	// matching the pre-lock behaviour of not saving a partially-applied index.
	let lock = wt.lock_index().await?;
	let result: Result<(), WorktreeError> = async {
		// Fold-keys the target keeps under an exact spelling that is *also* in the current index (a retained
		// path whose working file stays). A different-cased index entry that folds to the same key must NOT
		// have its working file removed — on a case-insensitive filesystem it is the *same* file as the
		// retained entry, so removing it would lose the retained (possibly locally-edited) content. This
		// arises only from a case-colliding index (`Foo` and `foo` both present, from a case-sensitive-FS
		// commit) that the target resolves to one spelling. Deliberate divergence (probed vs git 2.55): git
		// checks the shared file against the DROPPED entry's blob and, if clean, deletes it — which also
		// removes the KEPT entry's file (same inode), leaving that path tracked-but-missing; if dirty it
		// refuses. We instead always preserve BOTH the file and the colliding index entry, never deleting a
		// file a retained entry still tracks nor discarding staged content git keeps (probed vs git 2.55: the
		// index keeps both spellings). Safe for a triply-pathological, hand-crafted-index case.
		let retained_folds: HashSet<String> = current
			.keys()
			.filter(|path| target_paths.contains_key(path.as_str()))
			.map(|path| fold_key(path, fold))
			.collect();

		// Index entries (any stage) the target does not keep under the same exact path. Spanning all stages
		// lets a force checkout (e.g. `merge --abort`) discard leftover conflict stages too. Classify each:
		// - **collision** (fold-key shared with a retained entry): drop only the stale index entry — its
		//   working file belongs to the retained path;
		// - **case-rename** (fold-key present in the target under a different case): re-cased on disk;
		// - **true stray** (fold-key absent from the target): removed from the working tree and index.
		// Computed and lexically validated up front so a hostile index path (`../x`, `.git/…`) aborts with
		// the tree untouched.
		let candidates: std::collections::BTreeSet<String> = index
			.entries
			.iter()
			.map(|e| e.path.clone())
			.filter(|path| !target_paths.contains_key(path.as_str()))
			.collect();
		let mut collision = Vec::new();
		let mut preserved = Vec::new();
		let mut renamed_away = Vec::new();
		let mut stray = Vec::new();
		for path in candidates {
			let key = fold_key(&path, fold);
			if retained_folds.contains(&key) {
				collision.push(path);
			} else if preserve_folds.contains(&key) {
				preserved.push(path);
			} else if target_fold.contains(&key) {
				renamed_away.push(path);
			} else {
				stray.push(path);
			}
		}
		for path in stray
			.iter()
			.chain(&renamed_away)
			.chain(&collision)
			.chain(&preserved)
		{
			validate_path(path)?;
		}

		// A case-rename removes its stale-cased SOURCE before the write loop runs, so a missing/corrupt blob
		// anywhere in the checkout — the rename's own target OR an unrelated path materialised later — would
		// otherwise leave that source already gone (neither casing surviving). Preflight EVERY blob this
		// checkout will materialise before removing anything, aborting with the tree untouched, as git
		// validates the target objects before mutating. A preserved, unchanged, or sparse-excluded path
		// writes no file, so it needs no blob.
		for (path, mode, oid) in &target {
			if preserve_folds.contains(&fold_key(path, fold)) {
				continue;
			}
			if !force
				&& current
					.get(path)
					.is_some_and(|(cm, co)| cm == mode && co == oid)
			{
				continue;
			}
			let excluded = match sparse.as_ref() {
				Some(matcher) => !matcher.includes(path),
				None => index.entry(path).is_some_and(|entry| entry.skip_worktree),
			};
			if excluded {
				continue;
			}
			wt.repository().read_blob(*oid).await?;
		}
		// A case-colliding staged entry (`foo` beside a retained `Foo`, a *distinct* blob from a
		// case-sensitive-FS commit) is left ENTIRELY untouched — its working file (the retained path's inode)
		// AND its index entry. git preserves such an entry across a retaining switch (probed vs git 2.55: the
		// index keeps both `Foo` and `foo`, reported `AM foo`); dropping the index entry would silently discard
		// staged content git keeps. `collision` is still classified above so these paths stay out of
		// `renamed_away`/`stray`, whose loops would remove the shared file.
		let _keep_collision_untouched = &collision;
		for path in &renamed_away {
			remove_worktree_path(wt, path)?;
			index.remove(path);
		}

		for (path, mode, oid) in &target {
			// A staged case-rename preserved above is not overwritten by the target's other-cased spelling.
			if preserve_folds.contains(&fold_key(path, fold)) {
				continue;
			}
			// Without `force`, leave a path unchanged from the index alone — so a local edit to a file
			// the checkout does not touch (e.g. an unrelated dirty file during a merge) is preserved,
			// the way git does. `force` (re)writes everything, restoring such files. A case-rename
			// (`current.get` is exact) is never "unchanged", so it is rewritten under the new case.
			if !force
				&& current
					.get(path)
					.is_some_and(|(cm, co)| cm == mode && co == oid)
			{
				continue;
			}
			write_entry(wt, path, mode, *oid, &mut index, sparse.as_ref(), force).await?;
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

	let sparse = wt.sparse_checkout().await?;
	let mut index = wt.load_index().await?;
	let staged: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();

	// Refuse any changed path whose local state (index or work tree) would be overwritten. The work-tree
	// cleanliness check is skipped only for a *new* excluded addition (present in `to`, not a current
	// index entry): it is not materialised and there is no tracked content to protect, so an in-the-way
	// untracked file is left alone. An excluded path that IS a current index entry keeps the check —
	// git refuses the fast-forward when its present working-tree file is locally modified, since
	// re-omitting the path would discard that edit (probed vs git 2.50.1). The staged check always
	// applies — `write_entry` rewrites the index entry, and git refuses when it would discard divergent
	// staged content — and an excluded path being DELETED (absent from `to`) keeps both checks too.
	// Standard excludes for the untracked-in-the-way test. This fast-forward path is only ever driven by
	// an internal `merge`, which supplies no global excludes file, so resolve with `None` — `core.ignoreCase`
	// and `.git/info/exclude` are still honoured (read internally).
	let fold = crate::excludes::ignore_case(wt).await?;
	let base = crate::excludes::load_base(wt, None).await?;
	// Under `core.ignoreCase`, git's index is case-insensitive, so `from`/`to`/`staged` are compared by
	// fold-key: a `Foo`→`foo` case-rename is one path, not an add+delete. Without this the to-side spelling
	// looks like an untracked obstruction and a fast-forward git completes is wrongly refused.
	let from_fold: HashMap<String, &(String, ObjectId<H>)> =
		from.iter().map(|(k, v)| (fold_key(k, fold), v)).collect();
	let staged_fold: HashMap<String, &(String, ObjectId<H>)> =
		staged.iter().map(|(k, v)| (fold_key(k, fold), v)).collect();
	let from_paths: HashSet<&str> = from.keys().map(String::as_str).collect();
	let to_fold: HashSet<String> = to.keys().map(|p| fold_key(p, fold)).collect();
	// Fold-keys the index carries under a spelling `from` (HEAD) does not — a LOCALLY STAGED case-rename
	// (`Foo`->`foo`). git refuses a fast-forward that still writes such a fold-key (it would overwrite the
	// staged rename) but allows one that only deletes it (probed vs git 2.55: modifying the renamed file
	// aborts, deleting it fast-forwards, preserving the staged spelling). Used below to refuse the former and
	// to keep the staged file on the latter.
	let staged_recase_folds: HashSet<String> = if fold {
		staged
			.keys()
			.filter(|s| !from_paths.contains(s.as_str()))
			.map(|s| fold_key(s, fold))
			.filter(|k| from_fold.contains_key(k))
			.collect()
	} else {
		HashSet::new()
	};
	let mut would_overwrite = Vec::new();
	for &path in &changed {
		let key = fold_key(path, fold);
		// Prefer this path's EXACT staged/from entry, falling back to the folded lookup only when there is
		// no exact entry — a genuine `Foo`->`foo` case-rename, whose to-side spelling `foo` is not itself
		// staged. A case-colliding index (`Foo`+`foo`, different blobs) is then checked against each entry's
		// OWN blob, refusing when the shared working file is dirty relative to *any* (as git does), rather
		// than against an arbitrarily-kept colliding survivor, which made the verdict nondeterministic.
		// A staged case-rename conflicts with any incoming change that still writes its fold-key: refuse (git
		// aborts). A delete of the fold-key does not conflict and falls through to be applied below.
		if staged_recase_folds.contains(&key) && to_fold.contains(&key) {
			would_overwrite.push(path.to_owned());
			continue;
		}
		let current = staged.get(path).or_else(|| staged_fold.get(&key).copied());
		let from_here = from.get(path).or_else(|| from_fold.get(&key).copied());
		let untracked_addition = to.contains_key(path)
			&& !staged_fold.contains_key(&key)
			&& sparse
				.as_ref()
				.is_some_and(|matcher| !matcher.includes(path));
		if from_here != current || (!untracked_addition && !is_clean(wt, path, current, &base, fold)?) {
			would_overwrite.push(path.to_owned());
		}
	}
	if !would_overwrite.is_empty() {
		return Ok(would_overwrite);
	}

	// Apply the diff, removing before writing. A case-rename (`Foo`→`foo`) must re-case the working-tree
	// entry, not just the index: on a case-insensitive filesystem the to-side write reaches the same inode
	// through the new spelling without changing the directory entry's case (probed vs git 2.55, which
	// renames it), so the stale-cased file must go before the write recreates it. Removing *all* obsolete
	// paths first (not just case-renames) also keeps a type change atomic — a `thing` file that becomes a
	// `thing/child` directory must lose the file before the child is written, which a sorted single pass
	// does not guarantee. The preflight above already refused a dirty rename. Everything else (unrelated
	// staged/dirty entries) is left as-is.
	// Fold-keys the to-tree keeps under an exact spelling that is also staged (a retained path whose working
	// file stays): a different-cased staged entry folding to the same key must not have its working file
	// removed — on a case-insensitive filesystem it is the same file as the retained entry. This arises only
	// from a case-colliding index (`Foo` and `foo` both staged) the to-tree resolves to one spelling. As in
	// `run` above, this is a deliberate safe divergence: git would delete the file when it is clean vs the
	// dropped entry (orphaning the retained one); we always preserve both the file and the colliding index
	// entry, discarding neither the shared file nor staged content git keeps.
	let retained_folds: HashSet<String> = staged
		.keys()
		.filter(|path| to.contains_key(path.as_str()))
		.map(|path| fold_key(path, fold))
		.collect();
	let mut collision = Vec::new();
	let mut removals = Vec::new();
	let mut writes = Vec::new();
	for &path in &changed {
		if to.contains_key(path) {
			writes.push(path);
		} else if retained_folds.contains(&fold_key(path, fold)) {
			collision.push(path);
		} else if staged_recase_folds.contains(&fold_key(path, fold)) {
			// A to-side delete of a locally staged case-rename: the staged entry (a different spelling) owns the
			// shared inode, so keep the file — removing `Foo` would delete the staged `foo`. git preserves it.
			collision.push(path);
		} else {
			// A to-side deletion — a true removal or the stale side of a case-rename: remove the file.
			removals.push(path);
		}
	}
	// Validate every pending write's blob BEFORE removing anything. This phase removes before it writes
	// (case-renames and type changes require it), so a missing or unreadable target blob would otherwise
	// leave an already-removed source file gone after the merge errors — losing a case-rename's sole copy,
	// or leaving a true removal applied while the merge did not. Preloading aborts cleanly with the working
	// tree untouched, as git validates the target objects before mutating.
	for &path in &writes {
		let (_, oid) = to
			.get(path)
			.expect("a write path is present in the to-tree");
		wt.repository().read_blob(*oid).await?;
	}
	// A case-colliding entry (`foo` beside a retained `Foo`) is left ENTIRELY untouched — file and index —
	// as in `run` above: git preserves such a staged entry across a retaining fast-forward, so dropping its
	// index entry would silently discard content git keeps. It stays classified as `collision` only to keep
	// it out of `removals`, whose loop would delete the shared working file.
	let _keep_collision_untouched = &collision;
	for &path in &removals {
		remove_worktree_file(wt, path)?;
		index.remove(path);
	}
	for &path in &writes {
		// A fast-forward is non-destructive, so an out-of-cone path with an in-the-way file is preserved.
		let (mode, oid) = to
			.get(path)
			.expect("a write path is present in the to-tree");
		write_entry(wt, path, mode, *oid, &mut index, sparse.as_ref(), false).await?;
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
	base: &[DirIgnore],
	fold: bool,
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
		Some((mode, oid)) => {
			let expected = u32::from_str_radix(mode, 8).unwrap_or(0);
			Ok(matches!(
				blob_of(wt.work(), path, &meta)?,
				Some((woid, _)) if woid == *oid && effective_mode(&meta, expected) == expected
			))
		}
		// An untracked file sits where `to` adds a path: refuse unless it is ignored.
		None => path_ignored(wt.work(), path, base, fold),
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
	sparse: Option<&SparseCheckout>,
	force: bool,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	// Decide whether this path is excluded from the working tree. With an active sparse-checkout the
	// current patterns decide — git recomputes `skip_worktree` on checkout, so a newly introduced
	// excluded path is added skip-worktree and NOT materialised, an included one is written. With no
	// sparse matcher, an existing entry's `skip_worktree` bit is carried forward unchanged (a rebuild
	// must not silently clear it). `assume_valid` is always preserved.
	let prior = index.entry(path);
	let assume_valid = prior.is_some_and(|entry| entry.assume_valid);
	let excluded = match sparse {
		Some(matcher) => !matcher.includes(path),
		None => prior.is_some_and(|entry| entry.skip_worktree),
	};
	if excluded {
		// A file present at an excluded path is handled by the checkout's destructiveness (probed against
		// git 2.50.1):
		// - a present file with NO prior tracked entry (an untracked file, or a new excluded addition) is
		//   PRESERVED on a non-force checkout — untracked data is never destroyed to satisfy the patterns —
		//   leaving the bit CLEAR and the file reported modified;
		// - a present file at a PRIOR tracked entry is reconstructable here (the cleanliness preflight in
		//   `run`/`twoway_merge` already refused a dirty one), so a non-force checkout REMOVES it and omits
		//   the path, exactly as git re-omits a clean recreated file rather than leaving a spurious change;
		// - a FORCE checkout (`reset --hard`, a merge/rebase/cherry-pick/revert `--abort`, or `checkout -f`)
		//   REMOVES any present file and omits the path, as git's `-f` discards in-the-way content.
		// An absent path is simply omitted. A retained present file gets a *default* stat (never the prior
		// one), so `status`'s fast path re-hashes it against the new blob id rather than calling it clean.
		let present = wt.work().lstat(path)?.is_some();
		let preserve = present && !force && prior.is_none();
		if present && !preserve {
			remove_worktree_path(wt, path)?;
		}
		let stat = if preserve {
			crate::Stat::default()
		} else {
			prior.map(|entry| entry.stat).unwrap_or_default()
		};
		index.upsert(IndexEntry {
			stat,
			mode: u32::from_str_radix(mode, 8).unwrap_or(0o100644),
			oid,
			stage: 0,
			assume_valid,
			skip_worktree: !preserve,
			path: path.to_owned(),
		});
		return Ok(());
	}

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
		assume_valid,
		skip_worktree: false,
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
	base: &[DirIgnore],
	fold: bool,
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
		Some((mode, oid)) => {
			let expected = u32::from_str_radix(mode, 8).unwrap_or(0);
			match blob_of(wt.work(), path, &meta)? {
				Some((woid, _)) if woid == *oid && effective_mode(&meta, expected) == expected => Ok(()),
				_ => Err(WorktreeError::Conflict(path.to_owned())),
			}
		}
		// Untracked file in the way of a checked-out path — refuse unless it is `.gitignore`d
		// (ignored files are expendable, as git overwrites them).
		None if path_ignored(wt.work(), path, base, fold)? => Ok(()),
		None => Err(WorktreeError::UntrackedOverwrite(path.to_owned())),
	}
}

/// The membership key for `path` under `core.ignoreCase`: ASCII-lower-cased when `fold`, else `path`
/// unchanged. Folds index/target/worktree path identity the way git's case-insensitive index does.
/// The exact paths tracked by `HEAD`'s tree (empty when `HEAD` is unborn). Used only under
/// `core.ignoreCase` to tell a genuine branch case-rename (the current index spelling is HEAD's, so the
/// target's other-cased entry is a rename to apply) from a locally STAGED recase (the index spelling is
/// absent from HEAD, so the target's entry must not overwrite the staged rename). A single tree read,
/// gated on `fold` by the caller.
async fn head_tree_entries<F, W, H>(
	wt: &WorkTree<F, W, H>,
) -> Result<Vec<(String, String, ObjectId<H>)>, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let Some(commit) = repo.refs().resolve_head().await? else {
		return Ok(Vec::new());
	};
	let tree = repo.commit_tree(commit).await?;
	Ok(repo.read_tree(tree).await?)
}

fn fold_key(path: &str, fold: bool) -> String {
	if fold {
		path.to_ascii_lowercase()
	} else {
		path.to_owned()
	}
}

/// Whether `path` (a file) is expendable under git's standard excludes — the whole-tree `base` levels
/// (`core.excludesFile`, `.git/info/exclude`) plus the `.gitignore` rules from the work-tree root down
/// to its parent — folded under `core.ignoreCase` (`fold`). An **ignored ancestor directory** makes the
/// file ignored too (git never descends into an ignored directory), so — like [`first_untracked_under`]
/// and `WorkTree::ignored_report_path` — check each ancestor as a directory before the leaf. Matching
/// only the leaf would miss a `foo/`-style rule over `foo/bar` and wrongly refuse a checkout git allows
/// (probed vs git 2.55: checking out a tree that adds `foo/bar` overwrites an untracked `foo/bar` when
/// `foo/` is ignored).
fn path_ignored<W: WorkDirFs>(
	work: &W,
	path: &str,
	base: &[DirIgnore],
	fold: bool,
) -> Result<bool, WorktreeError> {
	let stack = ignore_prefix(work, path, base)?;
	let mut idx = 0;
	while let Some(next) = path[idx..].find('/') {
		let ancestor = &path[..idx + next];
		if ignore::is_ignored_fold(ancestor, true, &stack, fold) {
			return Ok(true);
		}
		idx += next + 1;
	}
	Ok(ignore::is_ignored_fold(path, false, &stack, fold))
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

/// Whether a non-directory (a regular file **or a symlink**) occupies one of `path`'s ancestor slots,
/// so materialising `path` would force [`ensure_parents`] to delete or write through it. git never
/// destroys such an untracked entry: it leaves it, writes nothing, and warns "already present ... not
/// updated despite sparse patterns" — probed against git 2.50.1 for both a regular file and a symlink.
/// A free (absent) ancestor needs no directory yet, so it is not a blocker. Skipping the write here is
/// also strictly safer than [`ensure_parents`]' `UnsafePath` error on a symlinked ancestor, which
/// would otherwise abort the reapply *after* the new config and pattern file were already persisted.
pub(crate) fn ancestor_blocked<W: WorkDirFs>(work: &W, path: &str) -> Result<bool, WorktreeError> {
	let parts: Vec<&str> = path.split('/').collect();
	let mut ancestor = String::new();
	for part in &parts[..parts.len().saturating_sub(1)] {
		if !ancestor.is_empty() {
			ancestor.push('/');
		}
		ancestor.push_str(part);
		match work.lstat(&ancestor)? {
			Some(meta) if meta.kind.is_dir() => {}
			Some(_) => return Ok(true),
			None => return Ok(false),
		}
	}
	Ok(false)
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
	fold: bool,
) -> Result<Option<String>, WorktreeError> {
	// A wholly-ignored directory is expendable — git doesn't descend into it.
	if ignore::is_ignored_fold(dir_rel, true, stack, fold) {
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
		if ignore::is_ignored_fold(&rel, is_dir, stack, fold) {
			continue; // ignored content is expendable
		}
		if is_dir {
			if let Some(hit) = first_untracked_under(work, &rel, tracked, stack, fold)? {
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
	base: &[DirIgnore],
	fold: bool,
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
				// Expendable if ignored — including via an ignored *ancestor directory* (`a/` over a file
				// at `a/foo`), so use the ancestor-aware `path_ignored` rather than a leaf-only match.
				let ignored = path_ignored(work, &ancestor, base, fold)?;
				return Ok((!ignored).then(|| ancestor.clone()));
			}
			Some(_) => {}
			None => return Ok(None),
		}
	}
	Ok(None)
}

/// Build the ignore stack for the ancestors of `dir_rel`: git's whole-tree `base` excludes
/// (`core.excludesFile`, `.git/info/exclude`) at the bottom, then the work-tree root's `.gitignore`
/// and that of each directory strictly above `dir_rel` — ready for matching paths at `dir_rel`.
pub(crate) fn ignore_prefix<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	base: &[DirIgnore],
) -> Result<Vec<DirIgnore>, WorktreeError> {
	let mut stack = base.to_vec();
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
