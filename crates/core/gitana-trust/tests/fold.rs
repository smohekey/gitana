//! Trust-root folding over an in-memory object source, with signed trust chains built in-crate
//! (deterministic ed25519 keys via `ssh-key`, real SSHSIG signatures).

use std::collections::HashMap;
use std::fmt;

use gitana_object::{
	Commit, HashAlgorithm, ObjectId, ObjectKind, Sha256, TreeEntry, encode_commit, encode_tree,
};
use gitana_trust::{
	ObjectSource, Policy, TrustError, fold_trust_root, fold_trust_root_anchored,
	verify_candidate_trust_update,
};
use ssh_key::private::Ed25519Keypair;
use ssh_key::{HashAlg, LineEnding, PrivateKey};

/// An in-memory object store implementing [`ObjectSource`].
struct MemSource<H: HashAlgorithm> {
	objects: HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
}

#[derive(Debug)]
struct MissingObject(String);

impl fmt::Display for MissingObject {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "object {} not found", self.0)
	}
}

impl std::error::Error for MissingObject {}

impl<H: HashAlgorithm> ObjectSource<H> for MemSource<H> {
	type Error = MissingObject;

	async fn read_object(&self, id: &ObjectId<H>) -> Result<(ObjectKind, Vec<u8>), MissingObject> {
		self
			.objects
			.get(id)
			.cloned()
			.ok_or_else(|| MissingObject(id.to_hex()))
	}
}

/// A deterministic ed25519 signing key from a one-byte seed.
fn key(seed: u8) -> PrivateKey {
	PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

/// A trust document JSON blob enrolling `keys` under `policy`.
fn trust_json(keys: &[&PrivateKey], policy: &str) -> Vec<u8> {
	let lines: Vec<String> = keys
		.iter()
		.map(|k| k.public_key().to_openssh().expect("openssh"))
		.collect();
	serde_json::to_vec(&serde_json::json!({
		"version": 1,
		"policy": policy,
		"keys": lines,
	}))
	.expect("serialize trust doc")
}

/// A helper that builds signed trust commits into a shared object store.
struct Builder<H: HashAlgorithm> {
	objects: HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
}

impl<H: HashAlgorithm> Builder<H> {
	fn new() -> Self {
		Self {
			objects: HashMap::new(),
		}
	}

	fn insert(&mut self, kind: ObjectKind, bytes: Vec<u8>) -> ObjectId<H> {
		let id = ObjectId::<H>::compute(kind, &bytes);
		self.objects.insert(id, (kind, bytes));
		id
	}

	/// Build a trust commit carrying `doc` in its tree at `trust.json`, signed by `signer`, with the
	/// given `parents`. Returns the commit id.
	fn commit(&mut self, signer: &PrivateKey, doc: &[u8], parents: Vec<ObjectId<H>>) -> ObjectId<H> {
		let blob = self.insert(ObjectKind::Blob, doc.to_vec());
		let tree = self.insert(
			ObjectKind::Tree,
			encode_tree(&[TreeEntry {
				mode: "100644".to_owned(),
				name: "trust.json".to_owned(),
				id: blob,
			}]),
		);
		let mut commit = Commit {
			tree,
			parents,
			author: "Trust <trust@example.com> 1700000000 +0000".to_owned(),
			committer: "Trust <trust@example.com> 1700000000 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "trust update\n".to_owned(),
		};
		let signature = signer
			.sign("git", HashAlg::Sha512, &encode_commit(&commit))
			.expect("sign")
			.to_pem(LineEnding::LF)
			.expect("pem");
		commit.signature = Some(signature.trim_end().to_owned());
		self.insert(ObjectKind::Commit, encode_commit(&commit))
	}

	fn source(self) -> MemSource<H> {
		MemSource {
			objects: self.objects,
		}
	}
}

#[tokio::test]
async fn folds_a_valid_chain_to_the_tip_root() {
	let (a, b) = (key(1), key(2));
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[&a], "warn"), vec![]);
	// The update, signed by `a` (trusted at the bootstrap), enrols `b` and raises the policy.
	let tip = build.commit(&a, &trust_json(&[&a, &b], "require"), vec![boot]);
	let source = build.source();

	let root = fold_trust_root(&source, tip).await.expect("fold");
	assert_eq!(root.policy, Policy::Require);
	assert_eq!(root.keys.len(), 2);
}

#[tokio::test]
async fn anchor_is_the_bootstrap_signer_not_a_merely_listed_key() {
	// The bootstrap enrols BOTH `a` and `b`, but is signed by `a`. A caller pinning the anchor must
	// see `a`'s fingerprint — pinning `b` (a listed but non-signing key) would be forgeable, since an
	// attacker could list `b`'s public key in a chain signed by their own key.
	let (a, b) = (key(1), key(2));
	let a_fingerprint = a
		.public_key()
		.key_data()
		.fingerprint(HashAlg::Sha256)
		.to_string();
	let b_fingerprint = b
		.public_key()
		.key_data()
		.fingerprint(HashAlg::Sha256)
		.to_string();
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[&a, &b], "warn"), vec![]);
	let source = build.source();

	let folded = fold_trust_root_anchored(&source, boot).await.expect("fold");
	assert_eq!(folded.anchor.as_str(), a_fingerprint);
	assert_ne!(folded.anchor.as_str(), b_fingerprint);
}

#[tokio::test]
async fn rejects_a_bootstrap_not_self_signed() {
	let (a, b) = (key(1), key(2));
	let mut build = Builder::<Sha256>::new();
	// The bootstrap enrols only `a` but is signed by `b`.
	let boot = build.commit(&b, &trust_json(&[&a], "warn"), vec![]);
	let source = build.source();

	assert!(matches!(
		fold_trust_root(&source, boot).await,
		Err(TrustError::UntrustedKey(_))
	));
}

#[tokio::test]
async fn rejects_an_empty_key_root() {
	let a = key(1);
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[], "warn"), vec![]);
	let source = build.source();

	assert!(matches!(
		fold_trust_root(&source, boot).await,
		Err(TrustError::EmptyTrustRoot)
	));
}

#[tokio::test]
async fn rejects_an_update_signed_by_an_untrusted_key() {
	let (a, b) = (key(1), key(2));
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[&a], "warn"), vec![]);
	// The update would enrol `b`, but is signed by `b`, who is not yet trusted.
	let tip = build.commit(&b, &trust_json(&[&a, &b], "warn"), vec![boot]);
	let source = build.source();

	assert!(matches!(
		fold_trust_root(&source, tip).await,
		Err(TrustError::UntrustedKey(_))
	));
}

#[tokio::test]
async fn candidate_update_accepts_a_fast_forward_and_bootstrap() {
	let (a, b) = (key(1), key(2));
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[&a], "warn"), vec![]);
	let tip = build.commit(&a, &trust_json(&[&a, &b], "require"), vec![boot]);
	let source = build.source();

	// Bootstrap adoption (no prior tip).
	let root = verify_candidate_trust_update(&source, None, boot)
		.await
		.expect("bootstrap");
	assert_eq!(root.keys.len(), 1);

	// Fast-forward from boot to tip.
	let root = verify_candidate_trust_update(&source, Some(boot), tip)
		.await
		.expect("fast-forward");
	assert_eq!(root.keys.len(), 2);
}

#[tokio::test]
async fn candidate_update_refuses_a_divergent_update() {
	let a = key(1);
	let mut build = Builder::<Sha256>::new();
	let boot = build.commit(&a, &trust_json(&[&a], "warn"), vec![]);
	// Two siblings off the bootstrap: neither descends from the other.
	let tip = build.commit(&a, &trust_json(&[&a], "require"), vec![boot]);
	let sibling = build.commit(&a, &trust_json(&[&a], "warn"), vec![boot]);
	let source = build.source();

	assert!(matches!(
		verify_candidate_trust_update(&source, Some(tip), sibling).await,
		Err(TrustError::TrustChain(_))
	));
}
