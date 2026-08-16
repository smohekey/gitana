//! Resolving a checked-out submodule's `HEAD` commit, so a gitlink (mode `160000`) index entry can be
//! compared against the commit actually checked out in the submodule. Shared by `ls-files` (`-m`) and
//! `status`: a submodule is "modified" iff this differs from the recorded commit (git ignores the
//! submodule's own dirty content by default).

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::WorkTree;

/// The commit id checked out in the submodule at `path` (its `HEAD`), or `None` when it cannot be
/// resolved. Handles the common modern layout — a `.git` *gitfile* whose `gitdir:` target lives directly
/// under the superproject's `.git/modules/…` (readable through the repository file store) — with `HEAD`
/// either detached (a bare id) or a symref resolved from a loose ref or `packed-refs`. An old-style
/// in-worktree `.git` directory, or a target outside this git dir's `.git/` — notably a submodule of a
/// *linked* worktree, stored under `.git/worktrees/<wt>/modules/…` — is left unresolved (a best-effort,
/// deliberately documented limitation; see TODO.md). An unresolved submodule is treated as unchanged
/// rather than a false `M`.
pub(crate) async fn submodule_head_oid<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	path: &str,
) -> Option<ObjectId<H>> {
	let gitfile = wt.work().read(&format!("{path}/.git")).ok()?;
	let target = std::str::from_utf8(&gitfile)
		.ok()?
		.strip_prefix("gitdir:")?
		.trim();
	let git_dir = resolve_module_gitdir(path, target)?;
	let store = wt.repository().objects().file_store();

	let head = store.read_path(&format!("{git_dir}/HEAD")).await.ok()?;
	let head = std::str::from_utf8(&head).ok()?.trim();
	let Some(refname) = head.strip_prefix("ref:").map(str::trim) else {
		// A detached `HEAD` is a bare object id.
		return ObjectId::from_hex(head).ok();
	};
	// A loose ref first, then `packed-refs`.
	if let Ok(bytes) = store.read_path(&format!("{git_dir}/{refname}")).await
		&& let Ok(text) = std::str::from_utf8(&bytes)
		&& let Ok(oid) = ObjectId::from_hex(text.trim())
	{
		return Some(oid);
	}
	let packed = store
		.read_path(&format!("{git_dir}/packed-refs"))
		.await
		.ok()?;
	std::str::from_utf8(&packed).ok()?.lines().find_map(|line| {
		let (oid, name) = line.split_once(' ')?;
		(name == refname)
			.then(|| ObjectId::from_hex(oid).ok())
			.flatten()
	})
}

/// Resolve a submodule gitfile's `gitdir:` `target` (relative to the submodule work-tree `path`) to a
/// path *under* the superproject `.git/` — returning it relative to that git dir (`modules/<name>`).
/// `None` for a target that escapes the work tree or does not live under `.git/` (an unhandled layout).
fn resolve_module_gitdir(path: &str, target: &str) -> Option<String> {
	let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
	for component in target.split('/') {
		match component {
			"" | "." => {}
			".." => {
				parts.pop()?;
			}
			other => parts.push(other),
		}
	}
	parts.join("/").strip_prefix(".git/").map(str::to_owned)
}
