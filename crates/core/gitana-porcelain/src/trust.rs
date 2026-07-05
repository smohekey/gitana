//! `trust` — manage a repository's `refs/gitana/trust` chain (see
//! `docs/hlds/secure-git-trust-signing.md`). Each update is a signed commit whose tree carries the
//! canonical trust document; the commit chain *is* the authorization chain. These operations build
//! and sign those commits, and — crucially — re-verify the candidate chain through the same
//! `gitana-trust` core the server enforces with **before** moving the local ref, so a local edit can
//! never install a root the server would reject.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, encode_commit};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use gitana_trust::{
	Policy, TRUST_DOCUMENT_PATH, TrustDocument, TrustRoot, fold_trust_root,
	verify_candidate_trust_update,
};

use crate::{Identity, Signer};

/// The ref holding a repository's trust state. Its commit chain is the authorization chain.
pub const TRUST_REF: &str = "refs/gitana/trust";

/// Bootstrap trust for a repository: write the first, self-signed trust commit enrolling
/// `signing_pubkey` (the OpenSSH public-key line of the key we sign with) under `policy`, and point
/// `refs/gitana/trust` at it. Returns the new tip.
///
/// Refuses if trust is already initialised (bootstrap happens once). Under [`Policy::Require`] a
/// single enrolled key is unsafe — losing it locks out the repository — so that is refused unless
/// `break_glass` is set (the design's explicit override); enrol a second key first instead. The
/// built commit is re-folded through [`verify_candidate_trust_update`] before the ref moves, so the
/// bootstrap is only adopted if it genuinely self-verifies.
pub async fn trust_init<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	policy: Policy,
	signing_pubkey: &str,
	break_glass: bool,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	if repo.refs().resolve(TRUST_REF).await?.is_some() {
		bail!(
			"trust is already initialised ({TRUST_REF} exists); use `add-key`/`remove-key` to change it"
		);
	}
	if policy == Policy::Require && !break_glass {
		bail!(
			"`--policy require` with a single enrolled key is unsafe: losing it locks the repository. \
			 Initialise with `--policy warn`, enrol a second key with `add-key`, or pass \
			 `--break-glass` to override."
		);
	}

	let document = TrustDocument::new(1, policy, vec![signing_pubkey.to_owned()]);
	let tip = write_trust_commit(
		repo,
		&document,
		Vec::new(),
		"gitana trust: bootstrap",
		identity,
		signer,
	)
	.await?;

	// Prove the chain before moving the ref: the bootstrap must be self-signed by a key in its own
	// root. If the signing key is not the one we enrolled, this refuses and the ref stays unset.
	verify_candidate_trust_update(repo, None, tip).await?;

	repo.refs().update_ref(TRUST_REF, tip, None).await?;
	let committer = identity.committer_or_default().await;
	repo
		.refs()
		.append_reflog(TRUST_REF, None, tip, &committer, "trust: bootstrap")
		.await?;
	Ok(tip)
}

/// The current effective trust root: fold the `refs/gitana/trust` chain into its [`TrustRoot`], or
/// `None` when the ref is unset (trust not configured). Folding verifies the whole chain, so a
/// tampered or unverifiable root surfaces here as an error rather than a value.
pub async fn trust_list<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<Option<TrustRoot>> {
	match repo.refs().resolve(TRUST_REF).await? {
		None => Ok(None),
		Some(tip) => Ok(Some(fold_trust_root(repo, tip).await?)),
	}
}

/// Write a signed trust commit: store `document` as `trust.json` in a fresh tree, then build a
/// commit over it (with `parents`), sign the exact bytes git signs, and write the signed object.
/// Does not move any ref — the caller verifies the candidate first.
async fn write_trust_commit<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	document: &TrustDocument,
	parents: Vec<ObjectId<H>>,
	subject: &str,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let blob = repo.write_blob(&document.to_json()).await?;
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: TRUST_DOCUMENT_PATH.to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await?;

	let author = identity.author().await?;
	let committer = identity.committer().await?;
	let mut commit = Commit {
		tree,
		parents,
		author,
		committer,
		signature: None,
		extra_headers: Vec::new(),
		message: format!("{subject}\n"),
	};
	// Sign the unsigned encoding (exactly what git signs), then attach the armor and write the final
	// object — matching `verify_commit`, which strips the `gpgsig` header back off to check it.
	let armor = signer.sign(&encode_commit(&commit)).await?;
	commit.signature = Some(armor);
	Ok(
		repo
			.objects()
			.write_object(ObjectKind::Commit, &encode_commit(&commit))
			.await?,
	)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_trust::TrustedKey;

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, fixture};

	#[tokio::test]
	async fn list_is_none_before_init() {
		let (_dir, wt) = fixture().await;
		assert!(trust_list(wt.repository()).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn init_bootstraps_a_self_signed_root_that_folds_back() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);

		let tip = trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		// The ref points at the new commit, and folding it back yields the enrolled key and policy —
		// proving the signed bootstrap commit actually verifies through the trust core.
		assert_eq!(repo.refs().resolve(TRUST_REF).await.unwrap(), Some(tip));
		let root = trust_list(repo).await.unwrap().unwrap();
		assert_eq!(root.policy, Policy::Warn);
		assert_eq!(root.keys.len(), 1);
		let enrolled = TrustedKey::from_openssh(&signer.public_line()).unwrap();
		assert_eq!(root.keys[0].id(), enrolled.id());
	}

	#[tokio::test]
	async fn init_refuses_a_second_bootstrap() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);
		trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let err = trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("already initialised"), "{err}");
	}

	#[tokio::test]
	async fn init_refuses_require_with_a_single_key_unless_break_glass() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);

		let err = trust_init(
			repo,
			Policy::Require,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("break-glass"), "{err}");
		// The refusal left the ref unset, so break-glass can then bootstrap it.
		assert!(repo.refs().resolve(TRUST_REF).await.unwrap().is_none());

		trust_init(
			repo,
			Policy::Require,
			&signer.public_line(),
			true,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();
		let root = trust_list(repo).await.unwrap().unwrap();
		assert_eq!(root.policy, Policy::Require);
	}
}
