//! Resolving a checked-out submodule's `HEAD` commit, so a gitlink (mode `160000`) index entry can be
//! compared against the commit actually checked out in the submodule. Shared by `ls-files` (`-m`) and
//! `status`: a submodule is "modified" iff this differs from the recorded commit (git ignores the
//! submodule's own dirty content by default).

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::{GitPath, GitPathComponent};

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
	path: &GitPath,
) -> Option<ObjectId<H>> {
	let git_name = GitPathComponent::from_utf8(".git").ok()?;
	let gitfile = wt.work().read(&path.join(&git_name)).ok()?;
	let target = trim_ascii(gitfile.strip_prefix(b"gitdir:")?);
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
fn resolve_module_gitdir(path: &GitPath, target: &[u8]) -> Option<String> {
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
			resolve_module_gitdir(&mount, b"../.git/modules/sub"),
			Some("modules/sub".to_owned())
		);
	}

	#[test]
	fn rejects_a_non_utf8_repository_store_key() {
		let mount = GitPath::from_utf8("sub").unwrap();
		assert_eq!(
			resolve_module_gitdir(&mount, b"../.git/modules/raw-\xff"),
			None
		);
	}
}
