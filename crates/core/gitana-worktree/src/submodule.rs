//! Resolving a checked-out submodule's `HEAD` commit, so a gitlink (mode `160000`) index entry can be
//! compared against the commit actually checked out in the submodule. Shared by `ls-files` (`-m`) and
//! `status`: a submodule is "modified" iff this differs from the recorded commit (git ignores the
//! submodule's own dirty content by default).

use std::path::{Component, Path, PathBuf};

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::WorkTree;

/// The commit id checked out in the submodule at `path` (its `HEAD`), or `None` when it cannot be
/// resolved. Handles the modern `.git` *gitfile* layout for both ordinary and linked worktrees: the
/// marker must resolve beneath this checkout's own `<git-dir>/modules/…`, and that namespace is read
/// through the repository file store. `HEAD` may be detached (a bare id) or a symref resolved from a
/// loose ref or `packed-refs`. An old-style in-worktree `.git` directory or a foreign marker is left
/// unresolved and is treated as unchanged rather than a false `M`.
pub(crate) async fn submodule_head_oid<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	path: &str,
) -> Option<ObjectId<H>> {
	let gitfile = wt.work().read(&format!("{path}/.git")).ok()?;
	let target = std::str::from_utf8(&gitfile)
		.ok()?
		.strip_prefix("gitdir: ")?
		.trim_end_matches(['\n', '\r']);
	if target.is_empty() {
		return None;
	}
	let git_dir = match wt.worktree_root() {
		Some(root) => resolve_located_module_gitdir(root, wt.git_dir(), path, target)?,
		None => resolve_legacy_module_gitdir(path, target)?,
	};
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

/// Resolve a marker using the native layout discovered for this worktree. The marker is accepted only
/// when it names a repository strictly below this checkout's own `<git-dir>/modules` namespace. The
/// returned file-store path deliberately starts at `modules/`; [`WorktreeFileStore`](
/// gitana_file_store_local::WorktreeFileStore) routes that complete namespace to the per-worktree store.
fn resolve_located_module_gitdir(
	worktree_root: &Path,
	git_dir: &Path,
	path: &str,
	target: &str,
) -> Option<String> {
	let target = Path::new(target);
	let resolved = if target.is_absolute() {
		target.to_path_buf()
	} else {
		worktree_root.join(path).join(target)
	};
	let resolved = lexical_normalize(&resolved)?;
	let modules = lexical_normalize(&git_dir.join("modules"))?;
	#[cfg(windows)]
	let suffix = gitana_fs_native::strip_path_prefix(&resolved, &modules)?;
	#[cfg(not(windows))]
	let suffix = resolved.strip_prefix(&modules).ok()?.to_path_buf();
	if suffix.as_os_str().is_empty()
		|| suffix
			.components()
			.any(|component| !matches!(component, Component::Normal(_)))
	{
		return None;
	}
	let suffix = suffix.to_str()?;
	#[cfg(windows)]
	let suffix = suffix.replace('\\', "/");
	#[cfg(not(windows))]
	let suffix = suffix.to_owned();
	Some(format!("modules/{suffix}"))
}

/// Descriptor-only callers do not have a native worktree-root path. Preserve their existing ordinary
/// repository support by resolving a marker lexically to a path below the worktree's `.git/` store.
fn resolve_legacy_module_gitdir(path: &str, target: &str) -> Option<String> {
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

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				if !normalized.pop() {
					return None;
				}
			}
			other => normalized.push(other.as_os_str()),
		}
	}
	Some(normalized)
}

#[cfg(all(test, unix))]
mod tests {
	use super::*;

	#[test]
	fn resolves_ordinary_and_linked_worktree_module_stores() {
		assert_eq!(
			resolve_located_module_gitdir(
				Path::new("/repo"),
				Path::new("/repo/.git"),
				"modules/one",
				"../../.git/modules/one",
			),
			Some("modules/one".to_owned())
		);
		assert_eq!(
			resolve_located_module_gitdir(
				Path::new("/repo/linked"),
				Path::new("/repo/main/.git/worktrees/linked"),
				"modules/one",
				"../../../main/.git/worktrees/linked/modules/one",
			),
			Some("modules/one".to_owned())
		);
	}

	#[test]
	fn rejects_foreign_or_root_module_markers() {
		for target in [
			"../../other/modules/one",
			"../../.git/modules",
			"../../.git/modules/../objects",
		] {
			assert_eq!(
				resolve_located_module_gitdir(
					Path::new("/repo"),
					Path::new("/repo/.git"),
					"modules/one",
					target,
				),
				None,
				"{target} must not resolve as a module repository"
			);
		}
	}
}
