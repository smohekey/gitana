//! Writing a merge's conflicted result into the working tree and index, for the merge-like
//! operations (merge, cherry-pick, revert, rebase).

use std::collections::{HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;
use gitana_repository::Repository;

use crate::{ConflictOverwriteError, WorkTree, WorktreeError};

/// [`WorkTree::write_conflicted_state`].
pub(crate) async fn write<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	ours_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	conflicts: &[GitPath],
	excludes_file: Option<&[u8]>,
) -> Result<(), WorktreeError> {
	let repository = wt.repository();
	let base = tree_entry_map(repository, base_tree).await?;
	let ours = tree_entry_map(repository, ours_tree).await?;
	let theirs = tree_entry_map(repository, theirs_tree).await?;
	// A conflicted path is materialised regardless of the sparse patterns (below), which would overwrite
	// local bytes the user has at that path — so refuse first, as git aborts a merge that would clobber
	// local changes. An out-of-cone conflict path recreated/edited on disk diverges from the index; catch
	// it before writing anything (an in-cone dirty conflict path is also refused by the checkout itself).
	// A conflicted SUBMODULE (gitlink) is EXEMPT: git records its base/ours/theirs stages even with a
	// populated mount present, never treating the submodule's own contents as local changes that block the
	// merge (the mount is opaque). Keyed on OURS only — the conflict fallback keeps ours, so the merged
	// result at the path is ours' entry: if ours is an ordinary file (a file-vs-gitlink type conflict),
	// `materialise_paths` rewrites it from ours' blob, so an unstaged edit to that file MUST still block the
	// merge (a gitlink on theirs alone does not make the path opaque). A gitlink `ours` is opaque and its
	// mount is preserved by `materialise_paths`, so exempting it cannot delete local data.
	let is_gitlink = |path: &GitPath| ours.get(path).is_some_and(|(mode, _)| *mode == 0o160000);
	let diverged = wt.diverged_tracked_content_paths().await?;
	let clobbered: Vec<GitPath> = conflicts
		.iter()
		.filter(|p| diverged.contains(*p) && !is_gitlink(p))
		.cloned()
		.collect();
	if !clobbered.is_empty() {
		return Err(WorktreeError::ConflictOverwrite(
			ConflictOverwriteError::new(clobbered),
		));
	}
	// The conflicted paths are materialised after the checkout below has saved the merge, so a refusal
	// there (a path not representable here, a blob gitana can't convert) must come now, before anything
	// changes.
	wt.check_materialise(merged_tree, conflicts).await?;

	// Two-tree merge from HEAD's tree (`ours_tree`) to the merged result (conflict markers included): the
	// index equals HEAD here (each caller guarantees a clean index before recording conflict stages below),
	// so this lays down the merged/marker content while preserving unrelated local work, sharing `switch`'s
	// lock-safe, D/F- and sparse-correct engine.
	wt.checkout_merge(ours_tree, merged_tree, excludes_file)
		.await?;
	// A conflict on an out-of-cone path would have been sparse-omitted by the checkout above, leaving the
	// `UU` marker file unwritten and the conflict unresolvable. Conflicts are incompatible with
	// skip-worktree, so vivify every conflicted path's merged (marker) content regardless of the sparse
	// patterns, as git does (an in-cone path was already written, so this is idempotent for it).
	// For a conflicted GITLINK, `materialise_paths` normally leaves the slot untouched, but a genuine
	// INCOMING gitlink — one whose merged side differs from ours, e.g. a modify/delete where theirs keeps
	// the gitlink — must still get an empty mount even when sparse-excluded (git: "Version theirs left in
	// tree"). Compute that set from the merged tree so a divergent-pointer conflict that keeps ours (merged
	// == ours) is NOT force-materialised, while a theirs-only/moved incoming gitlink is.
	let merged = tree_entry_map(repository, merged_tree).await?;
	let force_gitlink_mounts: HashSet<GitPath> = conflicts
		.iter()
		.filter(|path| {
			merged.get(*path).is_some_and(|(mode, _)| *mode == 0o160000)
				&& merged.get(*path) != ours.get(*path)
		})
		.cloned()
		.collect();
	wt.materialise_paths(merged_tree, conflicts, &force_gitlink_mounts)
		.await?;

	let mut index = wt.load_index().await?;
	for path in conflicts {
		index.record_conflict(
			path,
			base.get(path).copied(),
			ours.get(path).copied(),
			theirs.get(path).copied(),
		);
	}
	wt.save_index(&index).await?;
	Ok(())
}

/// A tree's entries as `path -> (mode, oid)`, for recording conflict stages.
pub(crate) async fn tree_entry_map<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	tree: ObjectId<H>,
) -> Result<HashMap<GitPath, (u32, ObjectId<H>)>, WorktreeError> {
	let mut map = HashMap::new();
	for (path, mode, oid) in repository.read_tree(tree).await? {
		let mode = u32::from_str_radix(&mode, 8).unwrap_or(0o100644);
		map.insert(path, (mode, oid));
	}
	Ok(map)
}
