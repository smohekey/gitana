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
mod commit_error;
pub mod conflict;
mod merge;
mod prune;
mod rebase;
mod remote;
mod revert;
mod signing;
mod tag;
mod trust;

pub use cherry_pick::{PickOutcome, abort_cherry_pick, cherry_pick, continue_cherry_pick};
pub use commit::{commit, commit_signed};
pub use commit_error::CommitError;
pub use gitana_git_http::Deepen;
pub use merge::{MergeOutcome, abort_merge, continue_merge, merge};
pub use prune::{gc, prune};
pub use rebase::{RebaseOutcome, abort_rebase, continue_rebase, rebase, skip_rebase};
pub use remote::{
	CloneReflog, FetchOutcome, FetchReflog, PushOutcome, PushResult, PushTags, TagFetch, clone,
	fetch, pull_upstream, push, push_signed,
};
pub use revert::{RevertOutcome, abort_revert, continue_revert, revert};
pub use tag::{tag, tag_signed};
pub use trust::{
	TRUST_REF, TrustSyncOutcome, trust_add_key, trust_init, trust_list, trust_remove_key,
	trust_set_policy, trust_sync,
};

/// Resolves the git identity lines (`Name <email> seconds ±hhmm`) for operations that record commits.
/// The engine never reads process env / config directly; the CLI adapter implements this over its
/// resolution. Methods are async so resolution stays lazy — an operation asks only once it is certain
/// to record a commit (e.g. after the empty-index guard).
pub trait Identity {
	/// The author line; errors if no identity is configured.
	async fn author(&self) -> Result<String>;
	/// The committer line; errors if no identity is configured.
	async fn committer(&self) -> Result<String>;
	/// The committer line for a reflog entry, defaulting a *missing* identity to a placeholder rather
	/// than failing — git records these (fast-forward, abort) without a configured identity. It still
	/// errors if the configuration itself cannot be loaded, so a malformed config aborts the operation.
	async fn committer_or_default(&self) -> Result<String>;
}

/// Produces a git-format SSH signature (an `SSHSIG` armor block, git's `git` namespace) over given
/// bytes. Like [`Identity`], the engine holds this capability rather than loading keys or invoking
/// tools itself: the CLI adapter implements it (over `ssh-keygen`), and an operation asks for a
/// signature only once it is certain to record a signed object.
pub trait Signer {
	/// Sign `payload`, returning the armored `-----BEGIN SSH SIGNATURE-----` block with no trailing
	/// newline — the value goes straight into a commit's `gpgsig` header, which
	/// [`gitana_object::encode_commit`] folds (a trailing newline would emit a stray blank
	/// continuation line and corrupt the signed object).
	async fn sign(&self, payload: &[u8]) -> Result<String>;
}

// The fixtures build a native cap-std `LocalFileStore` (`from_dir`), so the test module is
// native-only — keeping `--target wasm32-wasip2 --all-targets` clean.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod test_support {
	use std::cell::Cell;

	use anyhow::Result;
	use gitana_file_store_local::{CapWorkDir, LocalFileStore};
	use gitana_object::{ObjectId, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::{FileMode, Repository, TreeBuildEntry};
	use gitana_worktree::{Index, IndexEntry, Stat, WorkTree};
	use ssh_key::private::Ed25519Keypair;
	use ssh_key::{HashAlg, LineEnding, PrivateKey};

	use crate::{Identity, Signer};

	pub(crate) const WHO: &str = "A U Thor <a@example.com> 0 +0000";

	/// A test [`Signer`] over a deterministic ed25519 key: signs with the same SSHSIG recipe git uses
	/// (namespace `git`, SHA-512, LF-armored, trailing newline trimmed) so the signed objects it
	/// produces verify through the real `gitana-trust` core.
	pub(crate) struct TestSigner {
		key: PrivateKey,
	}

	impl TestSigner {
		/// A signer whose key is seeded by `seed` — distinct seeds give distinct keys.
		pub(crate) fn new(seed: u8) -> Self {
			Self {
				key: PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])),
			}
		}

		/// This signer's public key as an OpenSSH line, to enrol in a trust document.
		pub(crate) fn public_line(&self) -> String {
			self.key.public_key().to_openssh().expect("openssh line")
		}
	}

	impl Signer for TestSigner {
		async fn sign(&self, payload: &[u8]) -> Result<String> {
			Ok(
				self
					.key
					.sign("git", HashAlg::Sha512, payload)?
					.to_pem(LineEnding::LF)?
					.trim_end()
					.to_owned(),
			)
		}
	}

	/// A [`Signer`] whose `sign` always fails — models a signing failure (bad `gpg.format`, missing
	/// key, `ssh-keygen` error) for asserting a history operation leaves recoverable state.
	pub(crate) struct FailingSigner;

	impl Signer for FailingSigner {
		async fn sign(&self, _payload: &[u8]) -> Result<String> {
			anyhow::bail!("signing failed")
		}
	}

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
		async fn committer_or_default(&self) -> Result<String> {
			self.asked.set(true);
			Ok(WHO.to_owned())
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
		async fn committer_or_default(&self) -> Result<String> {
			Ok(WHO.to_owned())
		}
	}

	pub(crate) fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
		cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
	}

	/// A fresh repository (config + an unborn `main`) with a work tree, over a temp `LocalFileStore`.
	pub(crate) async fn fixture() -> (
		tempfile::TempDir,
		WorkTree<LocalFileStore, CapWorkDir, Sha256>,
	) {
		let dir = tempfile::TempDir::new().unwrap();
		let git_dir = dir.path().join(".git");
		std::fs::create_dir_all(&git_dir).unwrap();
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		repo.init().await.unwrap();
		let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(dir.path())), git_dir);
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
			skip_worktree: false,
			path: path.to_owned(),
		});
	}

	/// Commit `content` at `path` on the current branch via the porcelain `commit`, keeping the work
	/// tree on disk in sync (write the file, stage it, commit) so later operations see a clean tree.
	pub(crate) async fn commit_file(
		dir: &std::path::Path,
		wt: &WorkTree<LocalFileStore, CapWorkDir, Sha256>,
		path: &str,
		content: &[u8],
		identity: &TestIdentity,
	) -> ObjectId<Sha256> {
		std::fs::write(dir.join(path), content).unwrap();
		wt.add(&[path], "", false, None).await.unwrap();
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
