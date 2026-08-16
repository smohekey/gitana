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

use crate::CheckoutMode;
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
	mode: CheckoutMode<H>,
	excludes_file: Option<&str>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	// The two-tree merge is a distinct reconciliation (it preserves local divergences on paths the switch
	// does not touch); dispatch to it before the authoritative Reset/Overlay body below. A *missing* index,
	// though, has no staged state to preserve — git rebuilds it from the target like a full checkout — so
	// fall through to the authoritative (Overlay, `force == false`) body in that case.
	if let CheckoutMode::Merge { head } = mode
		&& wt.index_exists().await?
	{
		return merge_apply(wt, head, tree, excludes_file).await;
	}
	let force = matches!(mode, CheckoutMode::Reset);
	let target = wt.repository().read_tree(tree).await?;
	let target_paths: HashMap<&str, (&str, ObjectId<H>)> = target
		.iter()
		.map(|(path, mode, oid)| (path.as_str(), (mode.as_str(), *oid)))
		.collect();

	let sparse = wt.sparse_checkout().await?;
	// Take the index lock BEFORE reading the index (and hold it across the operation), so a concurrent index
	// writer completing between the read and a later lock cannot have its update discarded when the commit
	// writes back this snapshot. A pre-mutation refuse below drops `lock`, and its `Drop` releases
	// `index.lock` (the working tree still matches the index); the apply phase marks mutation and commits.
	let lock = wt.lock_index().await?;
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
		// Keyed by fold-key (see `merge_apply`) so a case-variant tracked file is recognised in the D/F
		// untracked-overwrite checks under `core.ignoreCase`.
		let tracked: HashSet<String> = current.keys().map(|p| fold_key(p, fold)).collect();
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
			// git treats a submodule mount as opaque — a CURRENT gitlink whose pointer is changing is never
			// refused on its own working-tree contents (an initialized submodule's files are not "untracked"
			// overwrites). Skip the mount-cleanliness scan below, mirroring `merge_apply`/`ensure_no_overwrite`;
			// the ancestor guard still runs. Keys on the CURRENT side, so an incoming gitlink over an ordinary
			// file/subtree is NOT exempt (that content is git's to protect).
			let current_is_gitlink = current.get(*path).is_some_and(|(cm, _)| cm == "160000");
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
			if !current_is_gitlink {
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
			}
			// A file->directory change removes a file occupying an ancestor slot; refuse if that
			// file is an untracked, non-ignored file (a tracked ancestor is validated by the
			// removal loop, an ignored one is expendable). Runs even for a gitlink (only the mount's OWN
			// contents are opaque — a submodule's untracked ancestor is still protected).
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

	// The index lock is already held (taken before the index read above); a held lock aborted the operation
	// there, before any filesystem change. On a mid-materialise failure the lock is released (not orphaned)
	// and the index is left unwritten, matching the pre-lock behaviour of not saving a partially-applied index.
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
			// A gitlink names a submodule commit, not a blob — nothing INCOMING to validate; `write_entry`
			// creates an empty mount directory rather than reading a blob. But installing the mount REMOVES
			// the outgoing content it supplants — an ordinary file at the exact slot, OR the descendants of a
			// subtree the gitlink replaces (`sub/file` under a target gitlink `sub`). Both the in-cone stray
			// removal AND `write_entry`'s excluded (out-of-cone) branch delete that content trusting it is
			// reconstructable from its blob, so every such OUTGOING blob must exist, else the sole clean copy is
			// lost — validate it BEFORE the sparse short-circuit below (mirrors `merge_apply`'s guard, which
			// validates the outgoing blob whether the incoming gitlink is in-cone or excluded).
			if mode == "160000" && !force {
				if let Some((cur_mode, cur_oid)) = current.get(path)
					&& cur_mode != "160000"
				{
					wt.repository().read_blob(*cur_oid).await?;
				}
				let prefix = format!("{path}/");
				for (cur_path, (cur_mode, cur_oid)) in &current {
					if cur_mode != "160000" && cur_path.starts_with(&prefix) {
						wt.repository().read_blob(*cur_oid).await?;
					}
				}
			}
			let excluded = match sparse.as_ref() {
				Some(matcher) => !matcher.includes(path),
				None => index.entry(path).is_some_and(|entry| entry.skip_worktree),
			};
			if excluded {
				continue;
			}
			// The incoming gitlink has no blob to validate (handled above); an ordinary target's blob does.
			if mode == "160000" {
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
		// The working tree is about to change: a cancellation from here must NOT release `index.lock` (the
		// tree would be left half-applied), so fail closed instead.
		lock.mark_mutation_started();
		for path in &renamed_away {
			remove_current_path(wt, path, current.get(path))?;
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
			remove_current_path(wt, path, current.get(path))?;
			index.remove(path);
		}
		// A subtree→gitlink transition removes the outgoing descendants (`sub/file` strays), and
		// `remove_empty_parents` then prunes the now-empty mount `sub/` that the earlier gitlink write left
		// in place. git keeps the empty mount, so recreate any in-cone target gitlink whose mount the removals
		// pruned — leaving the index (`160000`) and working tree consistent.
		for (path, mode, _) in &target {
			if *mode != "160000" || preserve_folds.contains(&fold_key(path, fold)) {
				continue;
			}
			let excluded = match sparse.as_ref() {
				Some(matcher) => !matcher.includes(path),
				None => index.entry(path).is_some_and(|entry| entry.skip_worktree),
			};
			if !excluded && wt.work().lstat(path)?.is_none() {
				ensure_parents(wt.work(), path)?;
				wt.work().create_dir(path)?;
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

/// git's two-tree merge (`read-tree -m -u`) from `head` to `target`: touch only the paths that differ
/// between the two trees, applying the change where the local (staged/working) state is clean or already
/// equals the target, and refusing where it conflicts — so a path the switch does not touch keeps whatever
/// staged or unstaged divergence it had (git carries local work across a branch switch). Backs `switch`.
///
/// Per changed path, with H=`head` entry, I=index (stage-0), T=`target` entry (`None` = a deletion):
///   * refuse when the index diverges from HEAD in a way the target also changes (`I != H && I != T`), or
///     the working file is dirty relative to the index (`!is_clean`);
///   * otherwise apply — write `T`, or remove when `T` is absent.
///
/// A written path additionally passes the untracked-overwrite guards (a directory or an untracked file in
/// the way). Unlike the authoritative [`run`] Reset/Overlay body, index entries the target does not mention
/// are left untouched, so staged additions survive.
async fn merge_apply<F, W, H>(
	wt: &WorkTree<F, W, H>,
	head: ObjectId<H>,
	target: ObjectId<H>,
	excludes_file: Option<&str>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let from = tree_map(wt.repository().read_tree(head).await?);
	let to = tree_map(wt.repository().read_tree(target).await?);

	// The only paths this update touches: those that differ between the two trees. Everything else — an
	// unrelated staged addition, a dirty file the switch does not touch — is left exactly as it is.
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
	// Take the index lock BEFORE reading the index, and hold it across the whole operation. Otherwise a
	// concurrent `add` (or any index writer) completing between this read and a later lock could have its
	// update silently discarded when the commit below writes back the snapshot loaded here. Every refuse-
	// phase early return drops `lock` before any worktree write, so its `Drop` releases `index.lock` (the
	// working tree still matches the index); the apply phase marks mutation and commits/releases explicitly.
	let lock = wt.lock_index().await?;
	let mut index = wt.load_index().await?;
	// A two-tree merge must not move `HEAD` while conflicts are unresolved — git refuses ("you need to
	// resolve your current index first" / "cannot switch branch while merging"), because the unmerged stages
	// would otherwise end up attached to a different branch. Refuse before examining or applying any diff
	// (even a same-commit switch, whose diff is empty, must not slip through).
	if index.has_conflicts() {
		return Err(WorktreeError::Unmerged);
	}
	let staged: HashMap<String, (String, ObjectId<H>)> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0)
		.map(|e| (e.path.clone(), (format!("{:o}", e.mode), e.oid)))
		.collect();
	// Intent-to-add placeholders (`git add -N`): git refuses a checkout/merge that would DROP one, even to
	// resolve a directory/file collision — its empty blob is not disposable staged content (probed vs git
	// 2.55: `add -N p/c` + target file `p` aborts on both `switch` and a fast-forward). An overwrite of one is
	// already caught by the generic staged-conflict check (the empty blob diverges from HEAD and the target),
	// but a D/F drop bypasses that, so it is refused explicitly below.
	let intent_to_add: HashSet<String> = index
		.entries
		.iter()
		.filter(|e| e.stage == 0 && e.intent_to_add)
		.map(|e| e.path.clone())
		.collect();
	let fold = crate::excludes::ignore_case(wt).await?;
	// Keyed by fold-key so the D/F untracked-overwrite checks recognise a case-variant tracked file: with
	// `core.ignoreCase` on a case-insensitive filesystem, HEAD's `P` and a target subtree `p/c` share the
	// `p` slot, and `p` must read as tracked (not an untracked ancestor that would abort the switch).
	let tracked: HashSet<String> = staged.keys().map(|p| fold_key(p, fold)).collect();
	// Strict ancestor directories of every staged path, precomputed once: the satisfied-deletion loop asks
	// "is any staged path nested under this deleted path?" — which is exactly "is this path a staged ancestor
	// dir?" — so an O(1) lookup replaces a per-deletion scan of the whole index (git switches with tens of
	// thousands of staged deletions would otherwise be O(deletions × index entries)).
	let staged_ancestor_dirs = strict_ancestor_dirs(staged.keys().map(String::as_str));
	let base = crate::excludes::load_base(wt, excludes_file).await?;
	// Under `core.ignoreCase` the index is case-insensitive, so `from`/`to`/`staged` compare by fold-key: a
	// `Foo`→`foo` case-rename is one path, not an add+delete.
	let from_fold: HashMap<String, &(String, ObjectId<H>)> =
		from.iter().map(|(k, v)| (fold_key(k, fold), v)).collect();
	let staged_fold: HashMap<String, &(String, ObjectId<H>)> =
		staged.iter().map(|(k, v)| (fold_key(k, fold), v)).collect();
	let from_paths: HashSet<&str> = from.keys().map(String::as_str).collect();
	let to_fold: HashSet<String> = to.keys().map(|p| fold_key(p, fold)).collect();
	// A locally STAGED case-rename (`Foo`→`foo`): the index carries a spelling `from` (HEAD) lacks. git
	// refuses a switch that still writes that fold-key (it would overwrite the staged rename) but allows one
	// that only deletes it. This is a rename only when HEAD's OWN spelling is no longer in the index — if the
	// index keeps `Foo` AND additionally stages `foo`, that is a colliding ADDITION, not a rename, and `Foo`
	// must be removed / `foo` carried normally.
	let head_spelling_staged: HashSet<String> = from
		.keys()
		.filter(|hp| staged.contains_key(hp.as_str()))
		.map(|hp| fold_key(hp, fold))
		.collect();
	let staged_recase_folds: HashSet<String> = if fold {
		staged
			.keys()
			.filter(|s| !from_paths.contains(s.as_str()))
			.map(|s| fold_key(s, fold))
			.filter(|k| from_fold.contains_key(k) && !head_spelling_staged.contains(k))
			.collect()
	} else {
		HashSet::new()
	};
	// Fold-keys with more than one stage-0 index entry (`Foo` AND `foo`) — a case-colliding index only
	// creatable by hand-crafted trees. The folded lookups above retain an arbitrary one of the colliding
	// entries, so a switch that touches such a key must be refused DETERMINISTICALLY (git refuses too) rather
	// than depending on which entry `HashMap` iteration happened to keep.
	let colliding_folds: HashSet<String> = if fold {
		let mut count: HashMap<String, u32> = HashMap::new();
		for k in staged.keys() {
			*count.entry(fold_key(k, fold)).or_default() += 1;
		}
		count
			.into_iter()
			.filter(|(_, n)| *n > 1)
			.map(|(k, _)| k)
			.collect()
	} else {
		HashSet::new()
	};
	// Paths whose EXACT index entry already equals the target: git treats them as satisfied, carrying any
	// unstaged work there and touching neither the index nor the working file — so a target blob staged over
	// an unstaged deletion stays deleted (` D`), not re-materialised. The match must be exact, not folded: a
	// case-only rename (index `Foo`, target `foo`, same blob) is NOT satisfied — the index still holds the
	// other spelling and the rename must be applied, not skipped.
	let satisfied: HashSet<&str> = changed
		.iter()
		.copied()
		.filter(|&path| staged.get(path) == to.get(path))
		.collect();
	// Paths the switch will materialise (changed, unsatisfied, present in the target), and the staged-ONLY
	// paths that directory/file-collide with one of them (`thing` written over a staged `thing/child`, or
	// the inverse). git's two-tree merge drops such colliding staged entries so the index stays valid — but
	// only when they are clean; a colliding staged path with an unstaged edit makes git abort rather than
	// lose that edit (checked in the refuse phase, applied in the apply phase).
	let write_paths: HashSet<&str> = changed
		.iter()
		.copied()
		.filter(|&p| !satisfied.contains(p) && to.contains_key(p))
		.collect();
	// HEAD paths the switch removes (present in `from`, absent from `to`). A staged-only path that D/F-collides
	// with one of these is dropped too: it needed the removed HEAD file to be a directory, so git discards it
	// when that file goes away (probed vs git 2.55: `D p` + staged `p/c`, switching to a tree without `p`, ends
	// with `p/c` gone).
	let removed_from: HashSet<&str> = from
		.keys()
		.map(String::as_str)
		.filter(|&p| !to.contains_key(p))
		.collect();
	// A staged-only path that directory/file-collides with a write is discarded (target wins) only when its
	// working file is PRESENT — a real on-disk collision. If it has an unstaged deletion (working file
	// absent), git PRESERVES the staged entry and does NOT materialise the colliding target path (probed vs
	// git 2.55: `D thing` / `AD thing/child`), so it must not be dropped, and the colliding write is skipped.
	// Only a STAGED-ONLY path — absent from both branch trees — gets this D/F reconciliation. A path in `from`
	// (HEAD) that the target replaces is an ordinary tree-diff change (a removal in `changed`), so it must
	// NOT be classified here, or its target replacement would be skipped and the switch would end with an
	// empty slot instead of the checked-out target.
	// Precompute ancestor-dir membership so every D/F check below is O(path depth), not O(paths) — an ordinary
	// large branch switch (thousands of non-colliding files) must not degrade to O(N²) prefix comparisons.
	let write_ancestor_dirs = strict_ancestor_dirs(write_paths.iter().copied());
	let removed_ancestor_dirs = strict_ancestor_dirs(removed_from.iter().copied());
	let mut df_removals: Vec<&str> = Vec::new();
	let mut df_preserved: HashSet<&str> = HashSet::new();
	for sp in staged.keys().map(String::as_str) {
		if to.contains_key(sp) || from.contains_key(sp) {
			continue;
		}
		// git resolves the staged path's fate by its ROLE in the D/F conflict and the collision slot's presence
		// (probed vs git 2.55, full 34-cell table):
		//   * a CHILD nested under a colliding write OR removed HEAD path — its collision slot (the colliding
		//     ancestor file's directory) PRESENT → target wins (discard); ABSENT → PRESERVED. Presence is read
		//     from that slot, not the child path, so an emptied directory still counts as present.
		//   * the PARENT of an incoming nested WRITE (`p/c` into a staged file `p`'s slot) — the write needs the
		//     slot, so the staged parent is discarded.
		//   * the PARENT of a merely REMOVED HEAD child — nothing competes for the slot, so it is PRESERVED.
		let child_of_write = ancestor_in(sp, &write_paths);
		let child_of_removed = ancestor_in(sp, &removed_from);
		let collider = child_of_write.or(child_of_removed);
		let parent_of_write = write_ancestor_dirs.contains(sp);
		if collider.is_none() && !parent_of_write {
			// Collides only as the parent of a removed child (or does not collide) → nothing to reconcile.
			if removed_ancestor_dirs.contains(sp) {
				df_preserved.insert(sp);
			}
			continue;
		}
		let discard = match collider {
			Some(c) => wt.work().lstat(c)?.is_some(), // slot present → target wins; absent → preserve
			None => true,                             // parent_of_write → always discarded
		};
		// ...but an OUT-OF-CONE (sparse-excluded) staged CHILD of an incoming write/removal is preserved rather
		// than discarded: git keeps out-of-cone staged content nested under a D/F collision instead of dropping
		// it for the incoming write (probed vs git 2.55: `git add --sparse x/c` + target file `x` leaves `x/c`
		// staged and skips the file). This is only the child case — an excluded staged PARENT of an incoming
		// write (`x` staged, target adds `x/c`) is still dropped, git installing the subtree over it (probed).
		let excluded =
			collider.is_some() && sparse.as_ref().is_some_and(|matcher| !matcher.includes(sp));
		if discard && !excluded {
			df_removals.push(sp);
		} else {
			df_preserved.insert(sp);
		}
	}
	// Ancestor dirs of the preserved paths, so a write's D/F collision with the preserved set is an
	// O(path-depth) lookup, not an O(preserved) pairwise scan: a write collides with a preserved path iff it
	// is an ancestor dir of one (`df_preserved_ancestor_dirs`) or nested under one (`ancestor_in`).
	let df_preserved_ancestor_dirs = strict_ancestor_dirs(df_preserved.iter().copied());

	// A satisfied DELETION — index and target agree the path is gone, but HEAD tracked it — must still refuse
	// if the working file was recreated as a non-ignored untracked file: git treats it as in the way (probed
	// vs git 2.55: it aborts rather than move HEAD and leave the untracked file). An absent or ignored file is
	// fine. (A satisfied path the target still keeps carries its unstaged work untouched, so it is not checked.)
	for &path in &satisfied {
		if !from.contains_key(path) || to.contains_key(path) {
			continue;
		}
		// A file present at `path` whose fold-key is still staged is the tracked file under its other case
		// (a `Foo`→`foo` rename the index carries): on a case-insensitive filesystem `lstat(Foo)` reaches
		// `foo`'s inode, so it is not a recreated untracked file and does not obstruct. But a DIRTY alias still
		// blocks the switch — git refuses (probed vs git 2.55 with a hand-crafted case-colliding index: staged
		// `foo`, HEAD `Foo`, target deletes the key, working `foo` edited). Check the shared inode against the
		// staged alias's blob rather than exempting it outright.
		if let Some(alias) = staged_fold.get(&fold_key(path, fold)) {
			if !is_clean(wt, path, Some(*alias), &base, fold)? {
				return Err(WorktreeError::Conflict(path.to_owned()));
			}
			continue;
		}
		// A staged subtree under this path (`D p` in HEAD/target but staged `p/c`) is not an in-the-way
		// untracked directory — git keeps the staged descendants and switches. `path` is a staged ancestor
		// dir iff some staged entry is nested under it (precomputed `staged_ancestor_dirs`).
		if staged_ancestor_dirs.contains(path) {
			continue;
		}
		// A non-directory occupying an ANCESTOR of this path blocks it — but only when that ancestor is
		// UNTRACKED (an in-the-way untracked file). A tracked ancestor owned by the target or index is the
		// legitimate file there, and git switches over it (probed vs git 2.55).
		{
			let mut anc = String::new();
			let parts: Vec<&str> = path.split('/').collect();
			for part in &parts[..parts.len().saturating_sub(1)] {
				if !anc.is_empty() {
					anc.push('/');
				}
				anc.push_str(part);
				if matches!(wt.work().lstat(&anc)?, Some(m) if !m.kind.is_dir())
					&& !staged.contains_key(anc.as_str())
					&& !to.contains_key(anc.as_str())
				{
					return Err(WorktreeError::UntrackedOverwrite(path.to_owned()));
				}
			}
		}
		// A recreated non-ignored thing at the path itself blocks the switch (probed vs git 2.55). A recreated
		// *directory* only blocks when it holds non-ignored untracked content — an empty or wholly-ignored one
		// git switches over (leaving it, or writing the target's subtree into it). A recreated file/symlink
		// blocks unless ignored. An absent path is fine.
		match wt.work().lstat(path)? {
			Some(meta) if meta.kind.is_dir() => {
				let mut stack = ignore_prefix(wt.work(), path, &base)?;
				if let Some(untracked) = first_untracked_under(wt.work(), path, &tracked, &mut stack, fold)?
				{
					return Err(WorktreeError::UntrackedOverwrite(untracked));
				}
			}
			Some(_) if !path_ignored(wt.work(), path, &base, fold)? => {
				return Err(WorktreeError::UntrackedOverwrite(path.to_owned()));
			}
			_ => {}
		}
	}

	// --- Refuse phase: no mutation. Reject any changed path whose local state conflicts with the switch. ---
	// A working non-directory sitting where the switch creates or destroys a directory `X/` is a
	// directory/file collision that git resolves ONLY under `core.ignoreCase` — it folds the name to clobber
	// the file. With case-sensitive `core.ignoreCase=false` git refuses (probed vs git 2.55: exactly the
	// matrix cells whose exit flips on the flag — a present staged/untracked file `p` where HEAD lacks a file
	// `p` and the switch adds or removes a subtree `p/…`). HEAD's OWN file at the slot (`from` tracks it) is
	// an ordinary file→directory change git resolves on either flag, so it is exempt. The candidate slots are
	// the directory ancestors the switch writes or removes — already computed — so this adds no whole-tree
	// scan, only an `lstat` per transitioning slot on the case-sensitive path.
	if !fold {
		for slot in write_ancestor_dirs.iter().chain(&removed_ancestor_dirs) {
			if from.contains_key(slot.as_str()) {
				continue;
			}
			if matches!(wt.work().lstat(slot.as_str())?, Some(meta) if !meta.kind.is_dir()) {
				return Err(WorktreeError::Conflict(slot.clone()));
			}
		}
	}
	for &path in &changed {
		if satisfied.contains(path) {
			// The index already equals the target, so unstaged work here is normally CARRIED. But when this
			// path participates in a D/F transition (its slot collides with an incoming write or a removed HEAD
			// path), a DIRTY working file blocks the switch — git refuses "local changes would be overwritten"
			// (probed vs git 2.55). A non-D/F satisfied path carries its unstaged edit as before.
			let in_df = write_ancestor_dirs.contains(path)
				|| under_any(path, &write_paths)
				|| removed_ancestor_dirs.contains(path)
				|| under_any(path, &removed_from);
			if in_df && staged.contains_key(path) && !is_clean(wt, path, staged.get(path), &base, fold)? {
				return Err(WorktreeError::Conflict(path.to_owned()));
			}
			continue;
		}
		let key = fold_key(path, fold);
		// A switch that WRITES a case-colliding stage-0 fold-key is refused deterministically (git refuses to
		// resolve which colliding entry the recase overwrites). A switch that only DELETES the fold-key does
		// not conflict — git removes the extra spellings and carries the rest — so it is left to normal handling.
		if colliding_folds.contains(&key) && to_fold.contains(&key) {
			return Err(WorktreeError::Conflict(path.to_owned()));
		}
		// A staged case-rename conflicts with any incoming change that still writes its fold-key.
		if staged_recase_folds.contains(&key) && to_fold.contains(&key) {
			return Err(WorktreeError::Conflict(path.to_owned()));
		}
		// The current index entry for this path. Fall back to a differently-cased staged entry ONLY when HEAD
		// (`from`) tracks the fold-key — a genuine case-rename context. When HEAD lacks the fold-key entirely,
		// a staged `foo` and a target `Foo` are two INDEPENDENT additions (a case-colliding addition), not the
		// same path, so they must not be conflated into an overwrite conflict; git keeps both and materialises
		// the target's spelling.
		let current = staged.get(path).or_else(|| {
			from_fold
				.contains_key(&key)
				.then(|| staged_fold.get(&key).copied())
				.flatten()
		});
		// Which staged entry OWNS the working file at `path` — a folded lookup that, unlike `current`, matches a
		// differently-cased staged addition even when HEAD lacks the fold-key. On a case-insensitive filesystem
		// the target's spelling and a staged `foo` share one inode, so for the CLEANLINESS check that file is
		// tracked (by `foo`), not untracked — while `current` stays exact so the conflict check still treats a
		// colliding addition (`foo` staged, `Foo` incoming) as two independent entries git keeps both of.
		let owner = staged.get(path).or_else(|| staged_fold.get(&key).copied());
		let from_here = from.get(path).or_else(|| from_fold.get(&key).copied());
		let to_here = to.get(path);
		// git treats a submodule mount as opaque — it never inspects a gitlink's own working tree for
		// checkout cleanliness. When the CURRENT tracked entry is a gitlink (a removal, or a pointer change),
		// skip the worktree-cleanliness refusals below: the apply phase records the index change and removes
		// an EMPTY mount directory, leaving a populated submodule in place (git warns "unable to rmdir" but
		// never refuses). This keys on the CURRENT side only — an INCOMING gitlink that replaces an ordinary
		// file or an untracked path is NOT exempt: that content is git's to protect, and git refuses to
		// overwrite a dirty/untracked file even to place a submodule (probed vs git 2.55).
		let is_gitlink = current.is_some_and(|(mode, _)| mode == "160000");
		// A new addition the sparse patterns exclude is added skip-worktree, not materialised, so an
		// in-the-way untracked file is left alone and no cleanliness applies. It must be a genuine ADDITION —
		// absent from HEAD (`from`) — not a path HEAD tracks whose index entry was staged-deleted: that is a
		// staged deletion the target would silently overwrite, which git REFUSES (probed vs git 2.55: out-of-
		// cone `D out/p`, target modifies out/p → "local changes would be overwritten"). Without the `from`
		// check this exemption would skip the staged-change conflict check and reinstate the target blob.
		let untracked_addition = to.contains_key(path)
			&& !from_fold.contains_key(&key)
			&& !staged_fold.contains_key(&key)
			&& sparse
				.as_ref()
				.is_some_and(|matcher| !matcher.includes(path));
		// This path is D/F-replaced by an incoming target write (`p/c` covered by a target file `p`, or a
		// target subtree covering a staged file): git lets the incoming file win the directory/file conflict
		// only when the working file is PRESENT — an absent working file means the staged edit would be lost,
		// so git refuses and the generic staged-conflict check below must run (probed vs git 2.55).
		let df_replaced = wt.work().lstat(path)?.is_some()
			&& (write_ancestor_dirs.contains(path)
				|| under_any(path, &write_paths)
				|| under_any(path, &removed_from));
		// A staged DELETION of a fold-key HEAD tracked, where the target re-provides it under a DIFFERENT case
		// (`git rm Foo`, target renames `Foo`→`foo`): git checks out the target rather than calling the deletion
		// a conflict (probed vs git 2.55, core.ignoreCase). Without this, folding `from_here` to HEAD's `Foo`
		// makes the generic check below see `Foo != None != foo` and refuse. The `from.get(path).is_none()` term
		// keeps this a RECASE only: when the target keeps HEAD's own spelling (`rm foo`, target modifies `foo`),
		// `from` has the exact path and the staged deletion IS a conflict git refuses — so this must not fire.
		let staged_deletion_recased = current.is_none()
			&& !from.contains_key(path)
			&& from_fold.contains_key(&key)
			&& !staged_fold.contains_key(&key)
			&& to_here.is_some();
		// A staged MODIFICATION of a HEAD-tracked file the target's D/F resolution would drop is refused when
		// dropping it loses that staged content (probed vs git 2.55, both `switch` and a fast-forward). Two
		// shapes qualify:
		//   * the file's OWN slot is turned into a directory (`write_ancestor_dirs`): `p` → `p/c` directly
		//     clobbers the staged file `p`;
		//   * an OUT-OF-CONE (sparse-excluded) child swept away when its parent subtree becomes a file
		//     (`x/c` staged out-of-cone via `add --sparse`, target replaces `x/` with file `x`).
		// The IN-CONE inverse — an in-cone staged child under a subtree the target replaces with a file — git
		// DOES drop (the `df_replaced` "target wins" path), as it does a staged addition or deletion.
		let staged_mod_of_tracked = from_here.is_some()
			&& current.is_some()
			&& from_here != current
			&& (write_ancestor_dirs.contains(path)
				|| sparse
					.as_ref()
					.is_some_and(|matcher| !matcher.includes(path)));
		if !untracked_addition {
			// Conflict when the index diverges from HEAD *and* is not already the target — a staged change the
			// switch would overwrite. (Not for a path an incoming D/F write replaces — unless that path carries a
			// staged modification git refuses to drop — nor a staged deletion the target's recase re-provides.)
			if (!df_replaced || staged_mod_of_tracked)
				&& !staged_deletion_recased
				&& from_here != current
				&& current != to_here
			{
				return Err(WorktreeError::Conflict(path.to_owned()));
			}
			// Working-tree cleanliness, dir-aware. A path the switch MATERIALISES whose slot is a *directory*
			// is a directory→file/symlink change: refuse only if the directory holds untracked, non-ignored
			// content (a clean tracked subtree is git's to replace) — `is_clean` would reject any directory
			// outright. Every other slot (a same-slot file, or absent) takes the standard cleanliness check.
			// Gitlinks are exempt: git never inspects a submodule's working tree here (see `is_gitlink`).
			if !is_gitlink {
				if to.contains_key(path)
					&& matches!(wt.work().lstat(path)?, Some(meta) if meta.kind.is_dir())
				{
					// But when `path` is itself TRACKED as a file (an index entry exists for it), a directory
					// there is an unstaged file→directory replacement — the tracked file has an unstaged deletion,
					// which git refuses (probed vs git 2.55). Refusing before the untracked-content scan matters:
					// otherwise the write would recursively clear the directory and silently destroy files inside
					// it, including IGNORED ones. Only a directory covering a tracked *subtree* (no entry for `path`
					// itself) reaches the content scan and is git's to replace when clean.
					if owner.is_some() {
						return Err(WorktreeError::Conflict(path.to_owned()));
					}
					let mut stack = ignore_prefix(wt.work(), path, &base)?;
					if let Some(untracked) =
						first_untracked_under(wt.work(), path, &tracked, &mut stack, fold)?
					{
						return Err(WorktreeError::UntrackedOverwrite(untracked));
					}
				} else if !is_clean(wt, path, owner, &base, fold)? {
					return Err(WorktreeError::Conflict(path.to_owned()));
				}
			}
			// A path the switch materialises must not sit under an untracked file (a file→directory change).
			// This ancestor guard applies to GITLINKS too: only the mount's OWN cleanliness is opaque to git;
			// it still refuses to clobber an untracked file to build a submodule's parent directory (probed vs
			// git 2.55 — losing that file would be data loss, which `ensure_parents` would otherwise cause by
			// unlinking it to create the mount).
			if to.contains_key(path)
				&& let Some(untracked) = untracked_file_ancestor(wt.work(), path, &tracked, &base, fold)?
			{
				return Err(WorktreeError::UntrackedOverwrite(untracked));
			}
		}
	}
	// A directory/file-colliding staged path is discarded by the apply below; refuse first if its working
	// file carries an unstaged edit, so the switch never silently deletes dirty content (git aborts). A
	// DIRECTORY occupying the slot is the incoming subtree's territory (the staged file is being replaced by
	// a subtree, its untracked siblings preserved), not a dirty copy of the staged file — so it does not block.
	for &sp in &df_removals {
		// A PRESENT intent-to-add placeholder must not be dropped for the incoming subtree — git refuses
		// regardless of whether its (empty-blob) working file happens to match, so the `is_clean` check below is
		// not enough. But an ABSENT placeholder (`git add -N x; rm x`) git DROPS during D/F resolution, so it
		// must not be refused (probed vs git 2.55: switching to a target with `x/c` succeeds).
		if intent_to_add.contains(sp) && wt.work().lstat(sp)?.is_some() {
			return Err(WorktreeError::Conflict(sp.to_owned()));
		}
		if matches!(wt.work().lstat(sp)?, Some(m) if m.kind.is_dir()) {
			continue;
		}
		if !is_clean(wt, sp, staged.get(sp), &base, fold)? {
			return Err(WorktreeError::Conflict(sp.to_owned()));
		}
	}
	// A PRESERVED staged D/F entry is kept as-is; but a dirty working file over it still blocks the switch
	// (git aborts "local changes would be overwritten"). An absent working file (the common preserve case)
	// is clean.
	for &sp in &df_preserved {
		if !is_clean(wt, sp, staged.get(sp), &base, fold)? {
			return Err(WorktreeError::Conflict(sp.to_owned()));
		}
	}

	// --- Apply phase. The index lock is already held (taken before the index read above). ---
	let result: Result<(), WorktreeError> = async {
		// Fold-keys the to-tree keeps under an exact spelling that is also staged (a retained path whose file
		// stays), and staged case-renames the to-tree deletes: the differently-cased staged entry owns the
		// shared inode, so its file must not be removed. Preserve both (as `run` does).
		let retained_folds: HashSet<String> = staged
			.keys()
			.filter(|path| to.contains_key(path.as_str()))
			.map(|path| fold_key(path, fold))
			.collect();
		let mut collision = Vec::new();
		let mut removals = Vec::new();
		let mut writes = Vec::new();
		let mut skipped_writes = Vec::new();
		for &path in &changed {
			// A satisfied path (index already equals the target) is left exactly as it is — index and working
			// file, including any unstaged divergence — so it is neither written nor removed.
			if satisfied.contains(path) {
				continue;
			}
			let key = fold_key(path, fold);
			if to.contains_key(path) {
				// A target path that directory/file-collides with a PRESERVED staged path (one with an unstaged
				// deletion) is not materialised — git leaves it as a pending change rather than clobber the
				// preserved staged entry. O(path depth): `path` collides iff it is an ancestor dir of a
				// preserved path, or nested under one.
				if df_preserved_ancestor_dirs.contains(path) || ancestor_in(path, &df_preserved).is_some() {
					// Not written, but HEAD still advances to the target — so its blob must exist (else the merge
					// publishes a commit referencing a missing object). Validate it below with the writes.
					skipped_writes.push(path);
					continue;
				}
				writes.push(path);
			} else if retained_folds.contains(&key) || staged_recase_folds.contains(&key) {
				collision.push(path);
			} else {
				removals.push(path);
			}
		}
		// Validate every path the apply phase will touch BEFORE it mutates — writes, plain removals, AND the
		// D/F-collision removals. `df_removals` in particular must be here: a crafted stage-0 path (`p/.git/x`
		// D/F-colliding with an incoming file `p`) is rejected by `remove_worktree_path`'s own guard mid-apply,
		// and by then a sibling removal may have run — a partial tree with a stranded `index.lock`. Rejecting it
		// up front keeps the working tree untouched.
		for path in removals.iter().chain(&writes).chain(&df_removals) {
			validate_path(path)?;
		}
		// Validate every pending write's blob BEFORE removing anything, so a missing/corrupt blob aborts with
		// the working tree untouched rather than after a case-rename source has been removed. A sparse-excluded
		// write materialises no file, but its blob is validated here too: a two-tree merge that moves an
		// in-cone file to an out-of-cone path would otherwise remove the source and record a skip-worktree entry
		// pointing at a missing blob, silently succeeding and — when the moved content had no other copy —
		// losing it. gitana has no partial clone, so a missing out-of-cone blob is corruption, not the
		// legitimate absence a partial checkout would tolerate; validate it and abort, as the (retired)
		// fast-forward path did. The cost is reading the changed out-of-cone blobs on such a switch, accepted
		// for the data-safety guarantee.
		for &path in &writes {
			let (mode, oid) = to
				.get(path)
				.expect("a write path is present in the to-tree");
			// A gitlink (submodule, mode 160000) names a COMMIT, not a blob in this object database — never
			// `read_blob` it. Whether OUT-OF-CONE (recorded index-only, materialising nothing) or IN-CONE
			// (recorded plus an empty mount directory, with no clone — `submodule update` would populate it),
			// the incoming side needs no blob. But when it REPLACES a present non-gitlink file — which
			// `write_entry`/`write_worktree_file` removes trusting it is reconstructable from its current blob
			// — that OUTGOING blob must exist, else the file is the sole surviving copy and the replacement
			// loses it. Validate it before any mutation.
			if mode == "160000" {
				if let Some((from_mode, from_oid)) = staged.get(path)
					&& from_mode != "160000"
				{
					wt.repository().read_blob(*from_oid).await?;
				}
				continue;
			}
			wt.repository().read_blob(*oid).await?;
		}
		// A D/F-collision removal drops a staged entry and deletes its (clean-by-hash) working file, so its
		// staged blob must exist before we destroy that file: if the blob is missing/corrupt while the working
		// copy still hashes to its OID, the file is the sole copy and removing it loses the content. Validate
		// each here, pre-mutation, aborting as the retired two-tree path did for this D/F state. (A staged
		// gitlink has no blob to validate.)
		for &path in &df_removals {
			if let Some((mode, oid)) = staged.get(path)
				&& mode != "160000"
			{
				wt.repository().read_blob(*oid).await?;
			}
		}
		// Validate the blob of every SATISFIED changed path (staged already equals the target, so it is not
		// written) and every plain REMOVAL's outgoing blob before advancing HEAD: a missing/corrupt blob would
		// otherwise let the merge succeed with HEAD pointing at content absent from the object database (the
		// retired two-tree path validated the satisfied blob, as a write), or delete the sole surviving copy of
		// a removed clean-by-hash file. gitlinks name a submodule commit, not a blob, so skip them.
		for &path in &satisfied {
			if let Some((mode, oid)) = to.get(path)
				&& mode != "160000"
			{
				wt.repository().read_blob(*oid).await?;
			}
		}
		for &path in &removals {
			if let Some((mode, oid)) = staged.get(path)
				&& mode != "160000"
			{
				wt.repository().read_blob(*oid).await?;
			}
		}
		// A D/F write suppressed by a preserved staged path is not materialised, but HEAD still advances to the
		// target — so its blob must exist too, or the merge publishes a commit referencing a missing object.
		for &path in &skipped_writes {
			if let Some((mode, oid)) = to.get(path)
				&& mode != "160000"
			{
				wt.repository().read_blob(*oid).await?;
			}
		}
		// Classify the sparse reconciliation of CARRIED unchanged out-of-cone entries BEFORE mutating. That
		// classification HASHES each excluded file, which can fail (an unreadable file, mode 000); running it
		// after `mark_mutation_started` — once the writes have landed — would leave a half-applied working tree
		// with a stranded `index.lock`. These entries are untouched by the writes/removals below (excluded by
		// the matcher, not among the handled writes/collisions, and not being removed), so their on-disk state,
		// and thus this classification, is identical now and after the apply. Applied (index bit + file removal)
		// after the mutations below.
		let sparse_reconcile: Vec<(String, crate::status::WorktreeContent)> =
			if let Some(matcher) = sparse.as_ref() {
				let handled: HashSet<&str> = writes.iter().chain(&collision).copied().collect();
				let gone: HashSet<&str> = removals.iter().chain(&df_removals).copied().collect();
				let file_mode = crate::status::worktree_file_mode(wt).await;
				let mut out = Vec::new();
				for entry in &index.entries {
					if entry.stage != 0
						|| handled.contains(entry.path.as_str())
						|| gone.contains(entry.path.as_str())
						|| matcher.includes(&entry.path)
					{
						continue;
					}
					// An intent-to-add placeholder is never "up to date" (git). A PRESENT one keeps its file and its
					// skip-worktree bit CLEAR — force `Diverged` so a previously-set bit (e.g. reconciled while
					// absent, then recreated) is cleared, rather than removing the empty-blob-matching file as
					// `worktree_content_state` would call it reconstructable. An ABSENT one is reconciled like any
					// excluded entry: git sets skip-worktree (retaining the intent flag), so its normal `Absent`
					// classification sets the bit and avoids a spurious ` D` in status (probed vs git 2.55).
					if entry.intent_to_add {
						let state = if wt.work().lstat(&entry.path)?.is_some() {
							crate::status::WorktreeContent::Diverged
						} else {
							crate::status::WorktreeContent::Absent
						};
						out.push((entry.path.clone(), state));
						continue;
					}
					let state = crate::status::worktree_content_state(wt, entry, file_mode).await?;
					out.push((entry.path.clone(), state));
				}
				out
			} else {
				Vec::new()
			};
		// A reconstructable reconcile entry is removed by `remove_worktree_path` in the apply phase, which
		// validates its path — but that runs after `mark_mutation_started`, so a crafted unsafe carried path
		// (`bad/.git/x`) would abort mid-apply and strand `index.lock` with a sibling write already landed.
		// Validate every reconcile path here, pre-mutation, to keep the half-apply invariant (conventions.md).
		for (path, _) in &sparse_reconcile {
			validate_path(path)?;
		}
		let _keep_collision_untouched = &collision;
		// The working tree is about to change: a cancellation from here must NOT release
		// `index.lock` (the tree would be left half-applied), so fail closed instead.
		lock.mark_mutation_started();
		for &path in &removals {
			if staged.get(path).is_some_and(|(mode, _)| mode == "160000") {
				remove_gitlink_mount(wt, path)?;
			} else {
				remove_worktree_file(wt, path)?;
			}
			index.remove(path);
		}
		// Directory/file collision resolution (git's two-tree merge lets the incoming target win): drop the
		// colliding staged-only entries (computed + cleanliness-checked in the refuse phase) so the index never
		// holds both a path and a path nested under it, which `write-tree`/commit rejects. Remove the working file
		// too, BEFORE the writes: `write_entry`'s `clear_dest`/`ensure_parents` handle a plain file/dir in the way,
		// but a staged *symlink* ancestor is rejected by `ensure_parents` as unsafe, so it must go first.
		for &path in &df_removals {
			remove_worktree_path(wt, path)?;
			index.remove(path);
		}
		for &path in &writes {
			let (mode, oid) = to
				.get(path)
				.expect("a write path is present in the to-tree");
			write_entry(wt, path, mode, *oid, &mut index, sparse.as_ref(), false).await?;
		}
		// Re-apply sparsity to CARRIED staged paths — those unchanged between the two trees, hence absent from
		// `changed` and never written/removed above. git applies the sparse patterns to the whole switched
		// index: a path staged out-of-cone (`git add --sparse`, which materialises it and clears skip-worktree)
		// is re-omitted on the switch — its clean working file removed and the skip-worktree bit re-set, the
		// staged blob preserved. The classification (`sparse_reconcile`) was computed in the pre-mutation
		// preflight above (its file hashing must not fail mid-apply); here we only apply it. Dirty → keep the
		// file and CLEAR the bit (git's "left despite sparse patterns", now an ordinary modification); absent →
		// omit (set the bit); clean → remove the reconstructable file and omit it.
		for (path, state) in &sparse_reconcile {
			if matches!(state, crate::status::WorktreeContent::Reconstructable) {
				// A reconstructable gitlink mount (an empty submodule directory) is rmdir'd via the mode-aware
				// helper; an ordinary reconstructable file is unlinked. `remove_worktree_path` leaves a directory
				// in place, so a gitlink mount would otherwise linger while the index records it skip-worktree.
				if index
					.entry(path)
					.is_some_and(|entry| entry.mode == 0o160000)
				{
					remove_gitlink_mount(wt, path)?;
				} else {
					remove_worktree_path(wt, path)?;
				}
			}
		}
		if !sparse_reconcile.is_empty() {
			let bits: HashMap<&str, bool> = sparse_reconcile
				.iter()
				.map(|(path, state)| {
					(
						path.as_str(),
						!matches!(state, crate::status::WorktreeContent::Diverged),
					)
				})
				.collect();
			for entry in index.entries.iter_mut() {
				if let Some(&bit) = bits.get(entry.path.as_str()) {
					entry.skip_worktree = bit;
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

/// The nearest strict ancestor of `path` that is a member of `set` (`p` for `p/c` when `p ∈ set`), or
/// `None`. O(path depth) — used so the D/F checks never scan all paths pairwise.
fn ancestor_in<'a>(path: &'a str, set: &HashSet<&str>) -> Option<&'a str> {
	path
		.char_indices()
		.filter(|&(_, ch)| ch == '/')
		.map(|(i, _)| &path[..i])
		.find(|anc| set.contains(anc))
}

/// Whether any strict ancestor of `path` is in `set`.
fn under_any(path: &str, set: &HashSet<&str>) -> bool {
	ancestor_in(path, set).is_some()
}

/// Every strict ancestor directory of every path in `paths` (`{a, a/b}` for `a/b/c`).
fn strict_ancestor_dirs<'a>(paths: impl Iterator<Item = &'a str>) -> HashSet<String> {
	let mut dirs = HashSet::new();
	for p in paths {
		for (i, ch) in p.char_indices() {
			if ch == '/' {
				dirs.insert(p[..i].to_owned());
			}
		}
	}
	dirs
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
/// [`ensure_no_overwrite`] but as a boolean for the two-tree merge's batch check.
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
		//   `run`/`merge_apply` already refused a dirty one), so a non-force checkout REMOVES it and omits
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
			// A materialised entry is really present now, never an `add -N` placeholder.
			intent_to_add: false,
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
		intent_to_add: false,
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

	if mode == "160000" {
		// A submodule (gitlink) names a commit, not a blob: git records the gitlink and creates an
		// empty mount directory, without cloning (`submodule update` would populate it). Never wipe an
		// already-checked-out submodule working tree — leave an existing directory in place; replace
		// only a plain file or symlink occupying the mount point.
		match wt.work().lstat(path)? {
			Some(meta) if meta.kind.is_dir() => {}
			Some(_) => {
				wt.work().remove_file(path)?;
				wt.work().create_dir(path)?;
			}
			None => wt.work().create_dir(path)?,
		}
		return Ok(());
	}

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
	// Unlink a file/symlink; a DIRECTORY at the slot is left in place (`remove_file` no-ops on it), the
	// way git only attempts `rmdir` and warns for a non-directory. A gitlink mount (a tracked path that
	// is a directory) is removed via the mode-aware [`remove_gitlink_mount`], NOT here — so an ordinary
	// tracked file the user replaced with a directory is never silently rmdir'd.
	let _ = wt.work().remove_file(path);
	remove_empty_parents(wt.work(), path);
	Ok(())
}

/// Remove `path` during a non-merge (`run`) checkout, honoring gitlink semantics. A CURRENT gitlink
/// (mode 160000) uses the rmdir-only [`remove_gitlink_mount`] — an empty mount is removed, but a
/// populated submodule OR a file/symlink the user placed at the slot is LEFT, as git only attempts
/// `rmdir` for a removed submodule and never unlinks that content. Anything else is an ordinary
/// file/symlink removal via [`remove_worktree_path`].
fn remove_current_path<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &str,
	current: Option<&(String, ObjectId<H>)>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	if current.is_some_and(|(mode, _)| mode == "160000") {
		remove_gitlink_mount(wt, path)
	} else {
		remove_worktree_path(wt, path)
	}
}

/// Like [`remove_worktree_path`], but reports a removal failure. An already-absent file is fine;
/// any other error (e.g. the path is now occupied by a directory) is returned so the caller can
/// refuse rather than silently leave the file in place. Validates `path` first (same escape guard
/// as [`remove_worktree_path`]), for the tree paths the two-tree merge removes.
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

/// Remove a submodule (gitlink) mount when its index entry goes away (a switch to a tree without the
/// gitlink, or an `--abort`). git removes the gitlink and, if the mount directory is EMPTY, deletes it;
/// a populated submodule working tree is LEFT in place (git only warns "unable to rmdir"). A mount the
/// user replaced with a plain file/symlink is removed like any file. Never errors on a non-empty
/// directory — unlike [`remove_worktree_file`], whose `remove_file` would fail on the mount directory.
fn remove_gitlink_mount<F, W, H>(wt: &WorkTree<F, W, H>, path: &str) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	validate_path(path)?;
	if has_symlinked_ancestor(wt.work(), path) {
		return Ok(());
	}
	// git only attempts `rmdir` on a removed submodule mount: an EMPTY directory is removed, and any
	// other occupant is LEFT in place (git warns "unable to rmdir" and continues) — a populated submodule
	// working tree, OR a file/symlink the user put at the slot. Never unlink that content: deleting a
	// file the user placed where the gitlink was is data loss git does not do (probed vs git 2.55).
	if matches!(wt.work().lstat(path)?, Some(meta) if meta.kind.is_dir()) {
		let _ = wt.work().remove_dir(path);
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
	// A submodule (gitlink) mount is opaque to checkout cleanliness — git never inspects a submodule's
	// own working tree — so a CURRENT gitlink is never an overwrite conflict here (a removal or pointer
	// change proceeds; the apply phase rmdir's only an empty mount, leaving a populated one). This mirrors
	// `merge_apply`'s `is_gitlink` exemption, for the non-merge (Overlay/WIT `checkout`) path.
	if matches!(current, Some((mode, _)) if mode == "160000") {
		return Ok(());
	}
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
	tracked: &HashSet<String>,
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
		} else if !tracked.contains(&fold_key(&rel, fold)) {
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
	tracked: &HashSet<String>,
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
				if tracked.contains(&fold_key(&ancestor, fold)) {
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
