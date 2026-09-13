//! Resolving a checked-out submodule's `HEAD` commit, so a gitlink (mode `160000`) index entry can be
//! compared against the commit actually checked out in the submodule. Shared by `ls-files` (`-m`) and
//! `status`: a submodule is "modified" iff this differs from the recorded commit (git ignores the
//! submodule's own dirty content by default).

use std::path::{Component, Path, PathBuf};

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::{GitPath, GitPathComponent};

use crate::WorkTree;

/// The commit id checked out in the submodule at `path` (its `HEAD`), or `None` when it cannot be
/// resolved. Handles the modern `.git` *gitfile* layout for both ordinary and linked worktrees: the
/// marker must resolve beneath this checkout's own `<git-dir>/modules/…`, and that namespace is read
/// through the repository file store. `HEAD` may be detached (a bare id) or a symref resolved from a
/// loose ref or `packed-refs`. An old-style in-worktree `.git` directory or a foreign marker is left
/// unresolved and is treated as unchanged rather than a false `M`.
pub(crate) async fn submodule_head_oid<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	path: &GitPath,
) -> Option<ObjectId<H>> {
	let git_name = GitPathComponent::from_utf8(".git").ok()?;
	let gitfile = wt.work().read(&path.join(&git_name)).ok()?;
	let target = trim_ascii(gitfile.strip_prefix(b"gitdir:")?);
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
	path: &GitPath,
	target: &[u8],
) -> Option<String> {
	let target = native_path(target)?;
	let resolved = if target.is_absolute() {
		target
	} else {
		worktree_root
			.join(native_path(path.as_bytes())?)
			.join(target)
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
fn resolve_legacy_module_gitdir(path: &GitPath, target: &[u8]) -> Option<String> {
	let mut parts: Vec<&[u8]> = path.components().collect();
	for component in target.split(|byte| *byte == b'/') {
		match component {
			b"" | b"." => {}
			b".." => {
				parts.pop()?;
			}
			other => parts.push(other),
		}
	}
	let mut resolved = Vec::new();
	for (index, part) in parts.iter().enumerate() {
		if index != 0 {
			resolved.push(b'/');
		}
		resolved.extend_from_slice(part);
	}
	let relative = resolved.strip_prefix(b".git/")?;
	Some(std::str::from_utf8(relative).ok()?.to_owned())
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Option<PathBuf> {
	use std::ffi::OsString;
	use std::os::unix::ffi::OsStringExt as _;
	Some(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Option<PathBuf> {
	Some(PathBuf::from(std::str::from_utf8(bytes).ok()?))
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
	while bytes.first().is_some_and(u8::is_ascii_whitespace) {
		bytes = &bytes[1..];
	}
	while bytes.last().is_some_and(u8::is_ascii_whitespace) {
		bytes = &bytes[..bytes.len() - 1];
	}
	bytes
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn resolves_a_module_below_a_raw_mount_path() {
		let mount = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		assert_eq!(
			resolve_legacy_module_gitdir(&mount, b"../.git/modules/sub"),
			Some("modules/sub".to_owned())
		);
	}

	#[test]
	fn rejects_a_non_utf8_repository_store_key() {
		let mount = GitPath::from_utf8("sub").unwrap();
		assert_eq!(
			resolve_legacy_module_gitdir(&mount, b"../.git/modules/raw-\xff"),
			None
		);
	}
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
mod located_tests {
	use super::*;

	#[test]
	fn resolves_ordinary_and_linked_worktree_module_stores() {
		assert_eq!(
			resolve_located_module_gitdir(
				Path::new("/repo"),
				Path::new("/repo/.git"),
				&GitPath::from_utf8("modules/one").unwrap(),
				b"../../.git/modules/one",
			),
			Some("modules/one".to_owned())
		);
		assert_eq!(
			resolve_located_module_gitdir(
				Path::new("/repo/linked"),
				Path::new("/repo/main/.git/worktrees/linked"),
				&GitPath::from_utf8("modules/one").unwrap(),
				b"../../../main/.git/worktrees/linked/modules/one",
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
					&GitPath::from_utf8("modules/one").unwrap(),
					target.as_bytes(),
				),
				None,
				"{target} must not resolve as a module repository"
			);
		}
	}
}
