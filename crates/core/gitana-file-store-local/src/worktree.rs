//! A [`FileStore`] that understands git's *linked worktree* layout.

use cap_std::fs::Dir;
use gitana_file_store::{ByteReader, DeleteOutcome, FileStore, Result, Version, WriteOutcome};

use crate::LocalFileStore;

/// A `FileStore` for a working tree that may be *linked* (created by `git worktree add`).
///
/// git splits such a repository's files in two: a per-worktree git directory holds the files that
/// are private to one checkout (`HEAD`, `index`, `ORIG_HEAD`, `MERGE_HEAD`/`MERGE_MSG`,
/// `logs/HEAD`), while a shared *common* directory holds everything else (`objects`, `refs/heads`,
/// `refs/tags`, `refs/remotes`, `packed-refs`, `config`). This store routes each git-relative path
/// to whichever underlying [`LocalFileStore`] owns it.
///
/// For an ordinary (non-linked) repository the two directories coincide, so the routing is a
/// transparent no-op — every path resolves to the same place either way.
pub struct WorktreeFileStore {
	common: LocalFileStore,
	worktree: LocalFileStore,
}

impl WorktreeFileStore {
	/// A store whose shared files live under the `common` capability and whose per-worktree files
	/// live under the `worktree` capability. Pass a clone of the same `Dir` for both for an ordinary
	/// single-directory repository.
	pub fn new(common: Dir, worktree: Dir) -> Self {
		Self {
			common: LocalFileStore::from_dir(common),
			worktree: LocalFileStore::from_dir(worktree),
		}
	}

	/// The underlying store that owns `path`: the per-worktree store for git's per-worktree files,
	/// the common store otherwise.
	fn store(&self, path: &str) -> &LocalFileStore {
		if is_per_worktree(path) {
			&self.worktree
		} else {
			&self.common
		}
	}
}

/// Whether a git-relative `path` is private to one worktree (lives in the worktree's own git dir)
/// rather than in the shared common dir. Follows git's worktree layout for the paths gitana touches:
/// `HEAD`/`index`, the in-progress operation state (`MERGE_HEAD`/`MERGE_MSG`, `CHERRY_PICK_HEAD`,
/// `REVERT_HEAD`, and gitana's `REBASE_*` files), and the per-worktree ref namespaces. gitana's other
/// refs (`refs/heads`, `refs/tags`, `refs/remotes`) are shared.
///
/// Routing the operation state per-worktree is critical: otherwise a rebase/cherry-pick/revert
/// started in one linked worktree would be visible — and `--abort`/`--continue`-able — from another,
/// mutating the wrong branch.
fn is_per_worktree(path: &str) -> bool {
	matches!(
		path,
		"HEAD"
			| "ORIG_HEAD"
			| "FETCH_HEAD"
			| "MERGE_HEAD"
			| "MERGE_MSG"
			| "CHERRY_PICK_HEAD"
			| "REVERT_HEAD"
			| "COMMIT_EDITMSG"
			| "index"
	) || path == "logs/HEAD"
		|| path.starts_with("REBASE_")
		|| path.starts_with("refs/worktree/")
		|| path.starts_with("refs/bisect/")
		|| path.starts_with("refs/rewritten/")
}

impl FileStore for WorktreeFileStore {
	fn read_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> {
		self.store(path).read_path(path)
	}

	fn read_path_versioned(&self, path: &str) -> impl Future<Output = Result<(Vec<u8>, Version)>> {
		self.store(path).read_path_versioned(path)
	}

	fn write_path_if_absent(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<WriteOutcome>> {
		self.store(path).write_path_if_absent(path, bytes)
	}

	fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> impl Future<Output = Result<Version>> {
		self.store(path).write_path_cas(path, bytes, expected)
	}

	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> {
		self.store(path).delete_path(path, expected)
	}

	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> {
		self.store(path).exists(path)
	}

	fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> {
		self.store(prefix).list_prefix(prefix)
	}

	fn read_path_range(
		&self,
		path: &str,
		offset: u64,
		length: u64,
	) -> impl Future<Output = Result<Vec<u8>>> {
		self.store(path).read_path_range(path, offset, length)
	}

	fn read_path_stream(&self, path: &str) -> impl Future<Output = Result<ByteReader>> {
		self.store(path).read_path_stream(path)
	}

	fn write_path_stream_if_absent(
		&self,
		path: &str,
		reader: ByteReader,
		max_len: u64,
	) -> impl Future<Output = Result<WriteOutcome>> {
		self
			.store(path)
			.write_path_stream_if_absent(path, reader, max_len)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_per_worktree_vs_shared_paths() {
		// Private to one checkout → the worktree dir: HEAD/index and the in-progress operation state
		// (merge, cherry-pick, revert, rebase), which must never be visible to another worktree.
		for path in [
			"HEAD",
			"ORIG_HEAD",
			"FETCH_HEAD",
			"MERGE_HEAD",
			"MERGE_MSG",
			"CHERRY_PICK_HEAD",
			"REVERT_HEAD",
			"REBASE_HEAD_NAME",
			"REBASE_ORIG_HEAD",
			"REBASE_ONTO",
			"REBASE_TODO",
			"COMMIT_EDITMSG",
			"index",
			"logs/HEAD",
			"refs/worktree/foo",
			"refs/bisect/bad",
			"refs/rewritten/onto",
		] {
			assert!(is_per_worktree(path), "{path} should be per-worktree");
		}
		// Shared across linked worktrees → the common dir.
		for path in [
			"config",
			"packed-refs",
			"objects/aa/bbcc",
			"refs/heads/main",
			"refs/tags/v1",
			"refs/remotes/origin/main",
			"logs/refs/heads/main",
		] {
			assert!(!is_per_worktree(path), "{path} should be shared");
		}
	}

	/// Ambient-open a directory for a test, creating it first (the store is capability-pure and
	/// requires an already-open `Dir`).
	fn open_dir(path: &std::path::Path) -> Dir {
		std::fs::create_dir_all(path).unwrap();
		Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
	}

	#[tokio::test]
	async fn routes_writes_to_the_owning_directory() {
		let dir = std::env::temp_dir().join(format!("wfs-route-{}", std::process::id()));
		let common = dir.join("common");
		let worktree = dir.join("wt");
		let _ = std::fs::remove_dir_all(&dir);
		let store = WorktreeFileStore::new(open_dir(&common), open_dir(&worktree));

		// A per-worktree file lands under the worktree store; a shared one under the common store.
		store
			.write_path_if_absent("HEAD", b"ref: refs/heads/main\n")
			.await
			.unwrap();
		store
			.write_path_if_absent("refs/heads/main", b"oid\n")
			.await
			.unwrap();

		assert_eq!(
			LocalFileStore::from_dir(open_dir(&worktree))
				.read_path("HEAD")
				.await
				.unwrap(),
			b"ref: refs/heads/main\n"
		);
		assert_eq!(
			LocalFileStore::from_dir(open_dir(&common))
				.read_path("refs/heads/main")
				.await
				.unwrap(),
			b"oid\n"
		);
		// The router reads them back through the same routing.
		assert!(store.exists("HEAD").await.unwrap());
		assert!(store.exists("refs/heads/main").await.unwrap());

		let _ = std::fs::remove_dir_all(&dir);
	}
}
