//! Porcelain operations — git's user-facing commands as engine functions that *return data* (the CLI
//! adapter renders it). Each is generic over the file store and hash algorithm, so operations are
//! backend-agnostic and unit-testable without driving the CLI.
//!
//! Operations that record commits take an [`Identity`] rather than reading process env / config
//! themselves: the CLI adapter implements it over its own `GIT_*` / `user.*` resolution, and the
//! engine asks for a signature only once a commit is certain to be made.
#![allow(async_fn_in_trait)]

use anyhow::Result;

mod cherry_pick;
mod commit;
pub mod conflict;
mod merge;
mod rebase;
mod remote;
mod revert;

pub use cherry_pick::{PickOutcome, abort_cherry_pick, cherry_pick, continue_cherry_pick};
pub use commit::commit;
pub use merge::{MergeOutcome, abort_merge, continue_merge, merge};
pub use rebase::{RebaseOutcome, abort_rebase, continue_rebase, rebase, skip_rebase};
pub use remote::{FetchOutcome, PushOutcome, clone, fetch, push};
pub use revert::{RevertOutcome, abort_revert, continue_revert, revert};

/// Resolves the git identity lines (`Name <email> seconds ±hhmm`) for operations that record commits.
/// The engine never reads process env / config directly; the CLI adapter implements this over its
/// resolution. Methods are async so resolution stays lazy — an operation asks only once it is certain
/// to record a commit (e.g. after the empty-index guard).
pub trait Identity {
	/// The author line; errors if no identity is configured.
	async fn author(&self) -> Result<String>;
	/// The committer line; errors if no identity is configured.
	async fn committer(&self) -> Result<String>;
	/// The committer line, falling back to a placeholder rather than failing — for reflog entries
	/// (fast-forward, abort) that git records without requiring a configured identity.
	async fn committer_or_default(&self) -> String;
}

// The fixtures build a native cap-std `LocalFileStore` (`from_dir`), so the test module is
// native-only — keeping `--target wasm32-wasip2 --all-targets` clean.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod test_support {
	use std::cell::Cell;

	use anyhow::Result;
	use gitana_file_store_local::LocalFileStore;
	use gitana_object::{ObjectId, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::{FileMode, Repository, TreeBuildEntry};
	use gitana_worktree::{Index, IndexEntry, Stat, WorkTree};

	use crate::Identity;

	pub(crate) const WHO: &str = "A U Thor <a@example.com> 0 +0000";

	/// A test [`Identity`] that yields a fixed signature and records whether it was ever asked to
	/// resolve — so a test can assert the engine did not resolve identity on a no-op path.
	#[derive(Default)]
	pub(crate) struct TestIdentity {
		pub asked: Cell<bool>,
	}

	impl Identity for TestIdentity {
		async fn author(&self) -> Result<String> {
			self.asked.set(true);
			Ok(WHO.to_owned())
		}
		async fn committer(&self) -> Result<String> {
			self.asked.set(true);
			Ok(WHO.to_owned())
		}
		async fn committer_or_default(&self) -> String {
			self.asked.set(true);
			WHO.to_owned()
		}
	}

	/// An [`Identity`] whose author/committer resolution always fails — for asserting a path that does
	/// not record a commit (e.g. a conflict that only materialises state) never resolves identity.
	pub(crate) struct FailingIdentity;

	impl Identity for FailingIdentity {
		async fn author(&self) -> Result<String> {
			anyhow::bail!("identity name not set")
		}
		async fn committer(&self) -> Result<String> {
			anyhow::bail!("identity name not set")
		}
		async fn committer_or_default(&self) -> String {
			WHO.to_owned()
		}
	}

	pub(crate) fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
		cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
	}

	/// A fresh repository (config + an unborn `main`) with a work tree, over a temp `LocalFileStore`.
	pub(crate) async fn fixture() -> (tempfile::TempDir, WorkTree<LocalFileStore, Sha256>) {
		let dir = tempfile::TempDir::new().unwrap();
		let git_dir = dir.path().join(".git");
		std::fs::create_dir_all(&git_dir).unwrap();
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		repo.init().await.unwrap();
		let wt = WorkTree::new(repo, dir.path().to_path_buf(), git_dir);
		(dir, wt)
	}

	/// Upsert a stage-0 entry for `oid` at `path`.
	pub(crate) fn stage(index: &mut Index<Sha256>, path: &str, oid: ObjectId<Sha256>) {
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid,
			stage: 0,
			assume_valid: false,
			path: path.to_owned(),
		});
	}

	/// Commit `content` at `path` on the current branch via the porcelain `commit`, keeping the work
	/// tree on disk in sync (write the file, stage it, commit) so later operations see a clean tree.
	pub(crate) async fn commit_file(
		dir: &std::path::Path,
		wt: &WorkTree<LocalFileStore, Sha256>,
		path: &str,
		content: &[u8],
		identity: &TestIdentity,
	) -> ObjectId<Sha256> {
		std::fs::write(dir.join(path), content).unwrap();
		wt.add(&[path], "").await.unwrap();
		crate::commit(wt, &format!("add {path}"), identity)
			.await
			.unwrap()
	}

	/// An off-branch commit of a single file (a sibling/child not on `main`), for divergent histories.
	/// Authored by [`WHO`].
	pub(crate) async fn loose_commit(
		repo: &Repository<LocalFileStore, Sha256>,
		parents: Vec<ObjectId<Sha256>>,
		path: &str,
		content: &[u8],
	) -> ObjectId<Sha256> {
		let blob = repo.write_blob(content).await.unwrap();
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: path.to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.unwrap();
		repo
			.create_commit(tree, parents, WHO, WHO, "loose\n")
			.await
			.unwrap()
	}
}
