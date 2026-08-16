//! The shared conflict lifecycle for the merge-like operations (merge, cherry-pick, revert, rebase):
//! detecting an in-progress operation, materialising a conflicted work tree and index, capturing the
//! resolved tree, and restoring the work tree on abort. Operations record this state and report the
//! conflicted paths as data; the CLI adapter renders the `CONFLICT` lines and decides the process's
//! fate (printing and the non-zero exit are policy, not engine).

use std::collections::HashMap;

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// The merge-like operation currently in progress (`merge` / `cherry-pick` / `revert` / `rebase`), or
/// `None` when the work tree is idle. The history-editing operations call this before starting so that
/// only one is ever underway, and each is concluded (`--continue`) or discarded (`--abort`) before
/// another begins.
pub async fn operation_in_progress<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
) -> Result<Option<&'static str>> {
	if repository.merge_head().await?.is_some() {
		return Ok(Some("merge"));
	}
	if repository.cherry_pick_head().await?.is_some() {
		return Ok(Some("cherry-pick"));
	}
	if repository.revert_head().await?.is_some() {
		return Ok(Some("revert"));
	}
	if repository.rebase_in_progress().await? {
		return Ok(Some("rebase"));
	}
	// A rebase started by *stock git* keeps its state under `rebase-merge/` (interactive/merge backend) or
	// `rebase-apply/` (am backend), not gitana's flat `REBASE_*` files — and once its conflicts are staged
	// the index has no unmerged entries, so neither `rebase_in_progress` nor an unmerged-index check would
	// notice it. git refuses to move HEAD while such a rebase is live (probed vs git 2.55: "cannot switch
	// branch while rebasing"), so detect both layout directories directly.
	let store = repository.objects().file_store();
	if store.is_dir("rebase-merge").await? || store.is_dir("rebase-apply").await? {
		return Ok(Some("rebase"));
	}
	Ok(None)
}

/// Write the merged result to the work tree (conflicted files carry markers) and record the conflict
/// stages (1/2/3 from base/ours/theirs) in the index. Refuses — before any caller records operation
/// state — if the checkout would clobber a touched local change.
pub async fn write_conflicted_state<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	merged_tree: ObjectId<H>,
	base_tree: ObjectId<H>,
	ours_tree: ObjectId<H>,
	theirs_tree: ObjectId<H>,
	conflicts: &[String],
) -> Result<()> {
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
	let is_gitlink = |path: &String| ours.get(path).is_some_and(|(mode, _)| *mode == 0o160000);
	let diverged = wt.diverged_tracked_content_paths().await?;
	let clobbered: Vec<&String> = conflicts
		.iter()
		.filter(|p| diverged.contains(*p) && !is_gitlink(p))
		.collect();
	if !clobbered.is_empty() {
		bail!(
			"your local changes to {clobbered:?} would be overwritten by the merge; commit or stash them first"
		);
	}

	// Two-tree merge from HEAD's tree (`ours_tree`) to the merged result (conflict markers included): the
	// index equals HEAD here (each caller guarantees a clean index before recording conflict stages below),
	// so this lays down the merged/marker content while preserving unrelated local work, sharing `switch`'s
	// lock-safe, D/F- and sparse-correct engine.
	wt.checkout_merge(ours_tree, merged_tree, None).await?;
	// A conflict on an out-of-cone path would have been sparse-omitted by the checkout above, leaving the
	// `UU` marker file unwritten and the conflict unresolvable. Conflicts are incompatible with
	// skip-worktree, so vivify every conflicted path's merged (marker) content regardless of the sparse
	// patterns, as git does (an in-cone path was already written, so this is idempotent for it).
	wt.materialise_paths(merged_tree, conflicts).await?;

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

/// The tree captured by the resolved index, refusing while unmerged stages remain. An empty index is
/// valid here (e.g. a delete/modify conflict resolved by deletion): `write_tree(&[])` is an empty
/// tree, unlike an ordinary commit which rejects it.
pub async fn resolved_tree<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<ObjectId<H>> {
	let index = wt.load_index().await?;
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}
	let entries = index.tree_entries();
	Ok(wt.repository().write_tree(&entries).await?)
}

/// The tree the index currently records (stage-0 entries only), assuming no unmerged stages. Used
/// to require a clean index before starting an operation (the index must equal `HEAD`).
pub async fn index_tree<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<ObjectId<H>> {
	let entries = wt.load_index().await?.tree_entries();
	Ok(wt.repository().write_tree(&entries).await?)
}

/// Restore the work tree and index to the (unmoved) `HEAD`, discarding conflict markers and unmerged
/// stages — the shared core of `--abort`. The caller clears its own operation state afterwards.
pub async fn restore_to_head<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<()> {
	let repository = wt.repository();
	let Some(head) = repository.refs().resolve_head().await? else {
		bail!("HEAD is unborn");
	};
	let head_tree = repository.commit_tree(head).await?;
	wt.checkout(head_tree, true, None).await?;
	Ok(())
}

/// A tree's entries as `path -> (mode, oid)`, for recording conflict stages.
async fn tree_entry_map<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	tree: ObjectId<H>,
) -> Result<HashMap<String, (u32, ObjectId<H>)>> {
	let mut map = HashMap::new();
	for (path, mode, oid) in repository.read_tree(tree).await? {
		let mode = u32::from_str_radix(&mode, 8).unwrap_or(0o100644);
		map.insert(path, (mode, oid));
	}
	Ok(map)
}

/// Ensure a commit message ends with a single trailing newline.
pub fn ensure_trailing_newline(message: String) -> String {
	if message.ends_with('\n') {
		message
	} else {
		format!("{message}\n")
	}
}
