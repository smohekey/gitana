//! A [`FileStore`] that understands git's *linked worktree* layout.

use std::path::PathBuf;

use gitana_file_store::{ByteReader, DeleteOutcome, FileStore, Result, Version, WriteOutcome};
use gitana_file_store_local::LocalFileStore;

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
pub struct WorktreeStore {
	common: LocalFileStore,
	worktree: LocalFileStore,
}

impl WorktreeStore {
	/// A store whose shared files live under `common_dir` and whose per-worktree files live under
	/// `worktree_dir`. Pass the same path for both for an ordinary single-directory repository.
	pub fn new(common_dir: impl Into<PathBuf>, worktree_dir: impl Into<PathBuf>) -> Self {
		Self {
			common: LocalFileStore::new(common_dir),
			worktree: LocalFileStore::new(worktree_dir),
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
/// rather than in the shared common dir. Follows git's worktree layout for the paths gitana
/// touches — gitana's other refs (`refs/heads`, `refs/tags`, `refs/remotes`) are all shared.
fn is_per_worktree(path: &str) -> bool {
	matches!(
		path,
		"HEAD" | "ORIG_HEAD" | "FETCH_HEAD" | "MERGE_HEAD" | "MERGE_MSG" | "COMMIT_EDITMSG" | "index"
	) || path == "logs/HEAD"
		|| path.starts_with("refs/worktree/")
		|| path.starts_with("refs/bisect/")
		|| path.starts_with("refs/rewritten/")
}

impl FileStore for WorktreeStore {
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
