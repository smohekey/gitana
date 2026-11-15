//! `commit` — record a commit from the staged index on the current branch.

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_worktree::WorkTree;

use crate::signing;
use crate::{CommitError, Identity, Signer};

/// Like [`WorkTree::commit`], but sign the commit (`gta commit -S`): the `gpgsig` armor covers the exact bytes
/// git signs, so stock git and the `gitana-trust` core both verify it. Otherwise identical — the same
/// refusals and lazy identity.
///
/// `signer.sign` is called only once a commit is certain to be made (after the
/// empty/unmerged/unchanged-index guards). A lazily-resolving signer (the CLI's `LazyCliSigner`) thus
/// defers loading the signing key until then, so a no-op `gta commit -S` reports "nothing to commit"
/// rather than a missing-signing-key error.
pub async fn commit_signed<F: FileStore, W: WorkDirFs, H: HashAlgorithm, S: Signer>(
	wt: &WorkTree<F, W, H>,
	message: &str,
	identity: &impl Identity,
	signer: &S,
) -> Result<ObjectId<H>, CommitError> {
	// The typed refusals (unmerged/empty/unchanged index) come from `prepare`; the remaining
	// build/sign/record step is CLI-only (the wasm boundary uses the unsigned `commit`), so its error
	// need not be finely typed — surface it through `Signing`.
	let prepared = wt.prepare_commit(message, identity).await?;
	signing::commit_on_head(
		wt.repository(),
		prepared.tree,
		&prepared.author,
		&prepared.committer,
		&prepared.message,
		Some(signer),
	)
	.await
	.map_err(CommitError::Signing)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_object::{ObjectKind, Sha256};
	use gitana_trust::{TrustedKey, verify_commit};
	use gitana_worktree::Index;

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, fixture, stage};

	#[tokio::test]
	async fn signs_a_commit_verifiable_by_the_trust_core() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();

		let signer = TestSigner::new(1);
		let public_line = signer.public_line();
		let id = commit_signed(&wt, "signed", &TestIdentity::default(), &signer)
			.await
			.unwrap();

		// The branch advanced to the signed commit, exactly as a plain commit would.
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(id)
		);

		// The raw object carries a signature that the real trust core accepts under the signing key —
		// so a `gta commit -S` object verifies the same way `git verify-commit` and receive-pack do.
		let (kind, raw) = wt.repository().objects().read_object(&id).await.unwrap();
		assert_eq!(kind, ObjectKind::Commit);
		let key = TrustedKey::from_openssh(&public_line).unwrap();
		let signer_id = verify_commit::<Sha256>(&raw, std::slice::from_ref(&key)).unwrap();
		assert_eq!(signer_id, key.id());

		// The message is preserved and the commit still points at the staged tree.
		let tree = wt.repository().commit_tree(id).await.unwrap();
		let entries = wt.repository().read_tree(tree).await.unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].2, blob);
	}

	#[tokio::test]
	async fn a_signed_commit_still_refuses_an_empty_index() {
		let (_dir, wt) = fixture().await;
		// The guards run before signing (and before identity): a no-op signed commit reports "nothing
		// to commit" without resolving identity or invoking the signer. `PanicSigner::sign` panics if
		// called — so a lazily-resolving signer (the CLI's `LazyCliSigner`) never loads its key, and
		// `gta commit -S` in a clean tree does not fail on a missing key first.
		let identity = TestIdentity::default();
		let err = commit_signed(&wt, "x", &identity, &PanicSigner)
			.await
			.unwrap_err();
		assert!(err.to_string().contains("nothing to commit"), "{err}");
		assert!(!identity.asked.get(), "identity resolved before the guard");
	}

	/// A [`Signer`] whose `sign` panics — to assert a path never reaches signing.
	struct PanicSigner;

	impl Signer for PanicSigner {
		async fn sign(&self, _payload: &[u8]) -> anyhow::Result<String> {
			panic!("signer invoked on a path that should not record a signed commit")
		}
	}

	#[tokio::test]
	async fn records_the_staged_tree_on_head() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();

		let id = wt.commit("first", &TestIdentity::default()).await.unwrap();

		// The branch now points at the commit, whose tree holds the staged blob.
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(id)
		);
		let tree = wt.repository().commit_tree(id).await.unwrap();
		let entries = wt.repository().read_tree(tree).await.unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].0, "f.txt");
		assert_eq!(entries[0].2, blob);
	}

	#[tokio::test]
	async fn refuses_an_empty_index_before_resolving_identity() {
		let (_dir, wt) = fixture().await;
		// The identity must not be resolved for a no-op commit (regression: it resolved first, so an
		// unconfigured identity masked "nothing to commit").
		let identity = TestIdentity::default();
		let err = wt.commit("x", &identity).await.unwrap_err();
		assert!(err.to_string().contains("nothing to commit"), "{err}");
		assert!(
			!identity.asked.get(),
			"identity resolved before the empty-index guard"
		);
	}

	#[tokio::test]
	async fn refuses_a_commit_that_does_not_change_the_tree() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();
		// The first commit establishes HEAD; the index still holds the same staged tree afterwards.
		wt.commit("first", &TestIdentity::default()).await.unwrap();

		// Re-committing the unchanged tree must refuse — and, like the empty-index guard, without
		// resolving identity (the check precedes it).
		let identity = TestIdentity::default();
		let err = wt.commit("again", &identity).await.unwrap_err();
		assert!(err.to_string().contains("nothing to commit"), "{err}");
		assert!(
			!identity.asked.get(),
			"identity resolved before the unchanged-tree guard"
		);
	}

	#[tokio::test]
	async fn refuses_an_unmerged_index() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"x\n").await.unwrap();
		let mut index = Index::<Sha256>::new();
		let path = gitana_path::GitPath::from_utf8("f.txt").unwrap();
		index.record_conflict(&path, Some((0o100644, blob)), None, None); // a stage-1 entry
		wt.save_index(&index).await.unwrap();

		let err = wt.commit("x", &TestIdentity::default()).await.unwrap_err();
		assert!(err.to_string().contains("unmerged files"), "{err}");
	}
}
