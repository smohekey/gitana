//! A [`FileStore`] that understands git's *linked worktree* layout.

use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
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
///
/// It is built over two already-open [`LocalFileStore`] capabilities, so it is target-agnostic:
/// two cap-std `Dir`s natively (via `new`), or two `wasi:filesystem` descriptors on wasm (via
/// `from_stores` / `single`).
pub struct WorktreeFileStore {
	common: Arc<LocalFileStore>,
	worktree: Arc<LocalFileStore>,
}

impl WorktreeFileStore {
	/// A store whose shared files live under the `common` store and whose per-worktree files live
	/// under the `worktree` store — git's linked-worktree split over two already-open capabilities
	/// (each a directory descriptor on wasm, a cap-std `Dir` natively).
	pub fn from_stores(common: LocalFileStore, worktree: LocalFileStore) -> Self {
		Self {
			common: Arc::new(common),
			worktree: Arc::new(worktree),
		}
	}

	/// A store over a single directory: an ordinary (non-linked) repository, where the per-worktree
	/// and common files coincide. Both routes share the one store, so its temp-file counter and
	/// per-path locks are shared — the two never contend over a `.tmp.<n>` name in the same directory.
	pub fn single(store: LocalFileStore) -> Self {
		let store = Arc::new(store);
		Self {
			common: Arc::clone(&store),
			worktree: store,
		}
	}

	/// A store over two cap-std directory capabilities (native). Pass a clone of the same `Dir` for
	/// both for an ordinary single-directory repository — or prefer `single` with one store to share
	/// its temp counter and locks.
	#[cfg(not(target_arch = "wasm32"))]
	pub fn new(common: Dir, worktree: Dir) -> Self {
		Self::from_stores(
			LocalFileStore::from_dir(common),
			LocalFileStore::from_dir(worktree),
		)
	}

	/// The shared **common** store — where `config`, `objects`, and shared refs live (for an ordinary
	/// repository it is the single store; for a linked worktree it is the main `.git`). git resolves the
	/// repository config and its relative `[include]` targets against this directory, so a consumer that
	/// reads config must read them here rather than through the per-path routing, which would send an
	/// include named like a per-worktree file (`config.worktree`, `HEAD`, …) to the wrong store.
	pub fn common(&self) -> &LocalFileStore {
		self.common.as_ref()
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
/// `HEAD` (and its ref-transaction `HEAD.lock`), `index` (and its `index.lock`), the in-progress
/// operation state (`MERGE_HEAD`/`MERGE_MSG`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, and gitana's
/// `REBASE_*` files), and the per-worktree ref namespaces. gitana's other refs (`refs/heads`,
/// `refs/tags`, `refs/remotes`) are shared.
///
/// Routing the operation state per-worktree is critical: otherwise a rebase/cherry-pick/revert
/// started in one linked worktree would be visible — and `--abort`/`--continue`-able — from another,
/// mutating the wrong branch. A `<path>.lock` always routes with `<path>` (a ref transaction or index
/// write must lock the *real* per-worktree file, interoperably with git's own `<path>.lock`) — so
/// `HEAD.lock`, `ORIG_HEAD.lock`, `index.lock`, etc. follow their targets, while a shared ref's lock
/// (`refs/heads/main.lock`) stays shared. Refs cannot themselves end in `.lock` (git forbids it), so
/// stripping the suffix is unambiguous.
fn is_per_worktree(path: &str) -> bool {
	if let Some(base) = path.strip_suffix(".lock") {
		return is_per_worktree(base);
	}
	// A one-level **pseudoref** — `HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, `MERGE_HEAD`, a custom `CUSTOM_REF`, … —
	// is per-worktree, matching git's `is_pseudoref_syntax` (a top-level name of only uppercase letters, digits,
	// and underscores). Enumerating just the well-known ones misroutes any other pseudoref (e.g. one a symbolic
	// `HEAD` points at) to the shared common dir, where it is not found — so the value silently reads back as
	// absent. `index`, `config.worktree`, `logs/HEAD`, `sharedindex.<oid>` are per-worktree too but are not
	// pseudoref-shaped (lowercase / `.` / `/`), so they are matched explicitly.
	if is_pseudoref(path) {
		return true;
	}
	path == "index"
		|| path == "config.worktree"
		|| path == "logs/HEAD"
		|| path.starts_with("REBASE_")
		|| path.starts_with("refs/worktree/")
		|| path.starts_with("refs/bisect/")
		|| path.starts_with("refs/rewritten/")
		// `sharedindex.<oid>` — a split index's shared base — lives beside the per-worktree `index` in the
		// worktree's own git dir, and `config.worktree` holds a linked worktree's `extensions.worktreeConfig`
		// overrides. Both are private to the checkout, not shared.
		|| path.starts_with("sharedindex.")
}

/// Whether `path` is a git **pseudoref**: a top-level name of only ASCII **uppercase letters, `_`, and `-`** —
/// git's `is_pseudoref_syntax` exactly (`isupper(c) || c == '_' || c == '-'`). Note this **excludes digits**,
/// so `CUSTOM-REF` is a pseudoref (per-worktree) but `CUSTOM1` is not (shared) — matching git's own routing.
/// Pseudorefs (`HEAD`, `ORIG_HEAD`, `MERGE_HEAD`, and any custom `SOME-REF`) live per-worktree, so they must
/// resolve against the worktree's own git dir. `REBASE_*` state files also match this shape (correct — they
/// are per-worktree too).
fn is_pseudoref(path: &str) -> bool {
	!path.is_empty()
		&& path
			.bytes()
			.all(|b| b.is_ascii_uppercase() || b == b'_' || b == b'-')
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

	fn write_path_replace(&self, path: &str, bytes: &[u8]) -> impl Future<Output = Result<()>> {
		self.store(path).write_path_replace(path, bytes)
	}

	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> {
		self.store(path).delete_path(path, expected)
	}

	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> {
		self.store(path).delete_path_unlocked(path)
	}

	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> {
		self.store(path).exists(path)
	}

	fn is_dir(&self, path: &str) -> impl Future<Output = Result<bool>> {
		self.store(path).is_dir(path)
	}

	fn remove_dir(&self, path: &str) -> impl Future<Output = Result<()>> {
		self.store(path).remove_dir(path)
	}

	fn size(&self, path: &str) -> impl Future<Output = Result<u64>> {
		self.store(path).size(path)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
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
			// The lock files must route with their targets, so a ref transaction / index write locks
			// the real per-worktree file (interoperably with git).
			"HEAD.lock",
			"ORIG_HEAD.lock",
			"index.lock",
			"logs/HEAD",
			"refs/worktree/foo",
			"refs/worktree/foo.lock",
			"refs/bisect/bad",
			"refs/rewritten/onto",
			// Any one-level *pseudoref* is per-worktree — git's `is_pseudoref_syntax`: uppercase letters, `_`, and
			// `-` (NOT digits). Not just the well-known ones: `AUTO_MERGE` (a real git pseudoref we don't
			// enumerate), a custom `CUSTOM-REF` (note the hyphen), or one a symbolic-ref chain resolves through.
			"AUTO_MERGE",
			"CUSTOM_REF",
			"CUSTOM-REF",
			"CUSTOM-REF.lock",
			"BISECT_EXPECTED_REV",
		] {
			assert!(is_per_worktree(path), "{path} should be per-worktree");
		}
		// Shared across linked worktrees → the common dir (including a shared ref's `.lock`). A top-level name
		// that is *not* pseudoref-shaped stays shared: lowercase / mixed-case, a `.`, or — matching git — a digit.
		for path in [
			"config",
			"packed-refs",
			"objects/aa/bbcc",
			"refs/heads/main",
			"refs/heads/main.lock",
			"refs/heads/UPPER", // uppercase, but not top-level (under refs/) → shared
			"refs/tags/v1",
			"refs/remotes/origin/main",
			"logs/refs/heads/main",
			"Mixed_Case", // has lowercase → not a pseudoref
			"gitk.cache", // lowercase + `.` → not a pseudoref
			"CUSTOM1",    // git's grammar excludes digits → shared
			"SHA256",     // digits → shared
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
