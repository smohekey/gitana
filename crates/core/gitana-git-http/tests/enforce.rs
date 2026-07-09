//! Pre-receive trust enforcement: a repo with a real signed trust root, pushes built from in-crate
//! SSHSIG-signed commits and push certificates, exercised across off/warn/require. The first block
//! drives the pure core (`verify_push`) directly; the `wire_*` block at the end drives the same
//! enforcement through `receive_pack`, asserting the verdict is rendered to report-status and refs
//! move (or don't) accordingly.

use std::collections::HashMap;

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{
	AuditEvent, CertCommand, NoReplayCheck, NonceLedger, PushCert, ReceiveOptions, ReceiveOutcome,
	RefUpdate, TrustContext, TrustVerdict, build_push_cert, make_nonce, receive_pack, verify_push,
	verify_push_with_ledger,
};
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, Sha256, Tag, TreeEntry, encode_commit, encode_pack,
	encode_tag, encode_tree, tag_signed_payload,
};
use gitana_object_store::ObjectStore;
use gitana_repository::{ReflogIntent, Repository};
use ssh_key::private::Ed25519Keypair;
use ssh_key::{HashAlg, LineEnding, PrivateKey};

type Repo = Repository<MemoryFileStore, Sha256>;
type Oid = ObjectId<Sha256>;
type Objects = HashMap<Oid, (ObjectKind, Vec<u8>)>;

const NOW: u64 = 1_700_000_000;
const SECRET: &[u8] = b"server-secret";
const REPO_ID: &str = "acme/app";
const PUSHEE: &str = "http://host/acme/app";
const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn key(seed: u8) -> PrivateKey {
	PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

async fn new_repo() -> Repo {
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(MemoryFileStore::new()));
	repo.init().await.expect("init");
	repo
}

fn context() -> TrustContext {
	TrustContext {
		nonce_secret: SECRET.to_vec(),
		repo_id: REPO_ID.to_owned(),
		pushee: PUSHEE.to_owned(),
		nonce_slop_secs: 900,
	}
}

/// A trust document enrolling `keys` under `policy`.
fn trust_json(keys: &[&PrivateKey], policy: &str) -> Vec<u8> {
	let lines: Vec<String> = keys
		.iter()
		.map(|k| k.public_key().to_openssh().expect("openssh"))
		.collect();
	serde_json::to_vec(&serde_json::json!({ "version": 1, "policy": policy, "keys": lines }))
		.expect("json")
}

/// The armored SSHSIG over `payload` in git's `git` namespace.
fn sign(signer: &PrivateKey, payload: &[u8]) -> String {
	signer
		.sign("git", HashAlg::Sha512, payload)
		.expect("sign")
		.to_pem(LineEnding::LF)
		.expect("pem")
		.trim_end()
		.to_owned()
}

fn empty_tree() -> Oid {
	ObjectId::compute(ObjectKind::Tree, &encode_tree::<Sha256>(&[]))
}

fn commit_bytes(signer: Option<&PrivateKey>, message: &str) -> (Oid, Vec<u8>) {
	let mut commit = Commit {
		tree: empty_tree(),
		parents: vec![],
		author: "Dev <dev@x> 1700000000 +0000".to_owned(),
		committer: "Dev <dev@x> 1700000000 +0000".to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: message.to_owned(),
	};
	if let Some(signer) = signer {
		commit.signature = Some(sign(signer, &encode_commit(&commit)));
	}
	let raw = encode_commit(&commit);
	(ObjectId::compute(ObjectKind::Commit, &raw), raw)
}

/// Write a bootstrap trust commit (self-signed by `signer`, enrolling `keys` under `policy`) into
/// the store and point `refs/gitana/trust` at it. Returns the tip.
async fn install_root(repo: &Repo, signer: &PrivateKey, keys: &[&PrivateKey], policy: &str) -> Oid {
	let objects = trust_commit(signer, keys, policy, vec![]);
	let mut tip = None;
	for (kind, raw) in objects.values() {
		let id = repo
			.objects()
			.write_object(*kind, raw)
			.await
			.expect("write");
		if *kind == ObjectKind::Commit {
			tip = Some(id);
		}
	}
	let tip = tip.expect("commit");
	repo
		.refs()
		.update_ref("refs/gitana/trust", tip, None, ReflogIntent::Skip)
		.await
		.expect("set trust ref");
	tip
}

/// The blob+tree+commit objects of a trust commit (keyed by id), the commit signed by `signer`.
fn trust_commit(
	signer: &PrivateKey,
	keys: &[&PrivateKey],
	policy: &str,
	parents: Vec<Oid>,
) -> Objects {
	let mut objects = Objects::new();
	let blob = trust_json(keys, policy);
	let blob_id = ObjectId::compute(ObjectKind::Blob, &blob);
	objects.insert(blob_id, (ObjectKind::Blob, blob));
	let tree = encode_tree(&[TreeEntry {
		mode: "100644".to_owned(),
		name: "trust.json".to_owned(),
		id: blob_id,
	}]);
	let tree_id = ObjectId::compute(ObjectKind::Tree, &tree);
	objects.insert(tree_id, (ObjectKind::Tree, tree));
	let mut commit = Commit {
		tree: tree_id,
		parents,
		author: "Trust <t@x> 1700000000 +0000".to_owned(),
		committer: "Trust <t@x> 1700000000 +0000".to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: "trust\n".to_owned(),
	};
	commit.signature = Some(sign(signer, &encode_commit(&commit)));
	let raw = encode_commit(&commit);
	objects.insert(
		ObjectId::compute(ObjectKind::Commit, &raw),
		(ObjectKind::Commit, raw),
	);
	objects
}

fn ref_update(name: &str, old: Option<Oid>, new: Oid) -> RefUpdate<Sha256> {
	RefUpdate {
		old,
		new: Some(new),
		name: name.to_owned(),
	}
}

fn ref_delete(name: &str, old: Oid) -> RefUpdate<Sha256> {
	RefUpdate {
		old: Some(old),
		new: None,
		name: name.to_owned(),
	}
}

/// A pushed-objects map for a single commit, including its (empty) tree — a real push carries every
/// reachable object, and the signing walk resolves the commit's reachability through the quarantine.
fn one_commit(commit: Oid, raw: Vec<u8>) -> Objects {
	let mut objects = Objects::new();
	objects.insert(empty_tree(), (ObjectKind::Tree, encode_tree::<Sha256>(&[])));
	objects.insert(commit, (ObjectKind::Commit, raw));
	objects
}

/// A signed annotated tag pointing at `target`; returns `(id, raw bytes)`.
fn signed_tag(signer: &PrivateKey, target: Oid, name: &str) -> (Oid, Vec<u8>) {
	let mut tag = Tag {
		object: target,
		kind: ObjectKind::Commit,
		name: name.to_owned(),
		tagger: Some("Dev <dev@x> 1700000000 +0000".to_owned()),
		signature: None,
		message: format!("{name}\n"),
	};
	tag.signature = Some(format!("{}\n", sign(signer, &tag_signed_payload(&tag))));
	let raw = encode_tag(&tag);
	(ObjectId::compute(ObjectKind::Tag, &raw), raw)
}

/// An *unsigned* annotated tag object pointing at `target`; returns `(id, raw bytes)`. A real tag
/// object (unlike a lightweight tag, which is a bare commit), but with no signature.
fn unsigned_tag(target: Oid, name: &str) -> (Oid, Vec<u8>) {
	let tag = Tag {
		object: target,
		kind: ObjectKind::Commit,
		name: name.to_owned(),
		tagger: Some("Dev <dev@x> 1700000000 +0000".to_owned()),
		signature: None,
		message: format!("{name}\n"),
	};
	let raw = encode_tag(&tag);
	(ObjectId::compute(ObjectKind::Tag, &raw), raw)
}

fn commit_id_of(objects: &Objects) -> Oid {
	objects
		.iter()
		.find(|(_, (kind, _))| *kind == ObjectKind::Commit)
		.map(|(id, _)| *id)
		.expect("commit")
}

/// A push certificate signed by `signer` for `commands`, with a fresh valid nonce by default.
fn signed_cert(signer: &PrivateKey, commands: Vec<CertCommand>, nonce: &str) -> PushCert {
	let mut cert = PushCert {
		version: "0.1".to_owned(),
		pusher: "Dev <dev@x> 1700000000 +0000".to_owned(),
		pushee: PUSHEE.to_owned(),
		nonce: nonce.to_owned(),
		push_options: Vec::new(),
		commands,
		signature: String::new(),
	};
	cert.signature = sign(signer, &cert.payload());
	cert
}

fn cert_command(new: Oid, name: &str) -> CertCommand {
	CertCommand {
		old: ZERO.to_owned(),
		new: new.to_hex(),
		refname: name.to_owned(),
	}
}

fn fresh_nonce() -> String {
	make_nonce(SECRET, REPO_ID, NOW, b"\x01\x02\x03\x04")
}

#[tokio::test]
async fn accepts_when_no_trust_root() {
	let repo = new_repo().await;
	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn accepts_when_policy_off() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "off").await;
	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn require_rejects_unsigned_commit_and_missing_cert() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { global, refs } => {
			assert!(global.is_some(), "certificate required globally");
			assert!(
				refs.iter().any(|(name, _)| name == "refs/heads/main"),
				"unsigned commit rejected per-ref: {refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn require_accepts_signed_commit_with_valid_cert() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn require_rejects_stale_nonce() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	// `now` well outside the slop window makes the nonce stale.
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		Some(&cert),
		NOW + 10_000,
	)
	.await
	.expect("verify");
	assert!(matches!(
		verdict,
		TrustVerdict::Reject {
			global: Some(_),
			..
		}
	));
}

/// An in-memory [`NonceLedger`]: records nonces in a set and reports a replay when one is seen twice.
#[derive(Default)]
struct MemLedger {
	seen: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl NonceLedger for MemLedger {
	type Error = std::convert::Infallible;

	async fn check_and_record(
		&self,
		nonce: &str,
		_expires_at: u64,
	) -> Result<bool, std::convert::Infallible> {
		// `insert` returns `true` when the nonce is new; a replay is a nonce already present.
		Ok(!self.seen.lock().unwrap().insert(nonce.to_owned()))
	}
}

#[tokio::test]
async fn require_rejects_a_replayed_nonce() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let commands = [ref_update("refs/heads/main", None, commit)];
	let ledger = MemLedger::default();

	// First use of this (fresh, valid) nonce is accepted and recorded.
	let first = verify_push_with_ledger(
		&repo,
		&context(),
		&commands,
		&objects,
		Some(&cert),
		NOW,
		&ledger,
	)
	.await
	.expect("verify");
	assert_eq!(first, TrustVerdict::Accept { warnings: vec![] });

	// Replaying the same still-fresh nonce is rejected, even though the certificate itself is valid.
	let second = verify_push_with_ledger(
		&repo,
		&context(),
		&commands,
		&objects,
		Some(&cert),
		NOW,
		&ledger,
	)
	.await
	.expect("verify");
	assert!(
		matches!(
			second,
			TrustVerdict::Reject {
				global: Some(_),
				..
			}
		),
		"{second:?}"
	);
}

#[tokio::test]
async fn warn_records_a_replayed_nonce_as_a_warning() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "warn").await;
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let commands = [ref_update("refs/heads/main", None, commit)];
	let ledger = MemLedger::default();

	verify_push_with_ledger(
		&repo,
		&context(),
		&commands,
		&objects,
		Some(&cert),
		NOW,
		&ledger,
	)
	.await
	.expect("verify");
	// Under `warn` the replay is recorded but not enforced — surfaced as a warning, not a rejection.
	let second = verify_push_with_ledger(
		&repo,
		&context(),
		&commands,
		&objects,
		Some(&cert),
		NOW,
		&ledger,
	)
	.await
	.expect("verify");
	match second {
		TrustVerdict::Accept { warnings } => {
			assert!(
				warnings.iter().any(|w| w.contains("replay")),
				"{warnings:?}"
			);
		}
		other => panic!("expected accept-with-warning, got {other:?}"),
	}
}

#[tokio::test]
async fn require_rejects_cert_command_mismatch() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	// The certificate signs a different ref than the push updates.
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/other")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	assert!(matches!(
		verdict,
		TrustVerdict::Reject {
			global: Some(_),
			..
		}
	));
}

#[tokio::test]
async fn require_rejects_commit_by_untrusted_key() {
	let (a, b) = (key(1), key(2));
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// The commit is signed, but by `b`, who is not in the trust root.
	let (commit, raw) = commit_bytes(Some(&b), "signed by untrusted\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/heads/main"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn warn_records_warnings_but_accepts() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "warn").await;
	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Accept { warnings } => assert!(!warnings.is_empty(), "warnings recorded"),
		other => panic!("expected accept-with-warnings, got {other:?}"),
	}
}

#[tokio::test]
async fn accepts_a_valid_candidate_trust_update() {
	let (a, b) = (key(1), key(2));
	let repo = new_repo().await;
	let bootstrap = install_root(&repo, &a, &[&a], "require").await;
	// A new trust commit (signed by the trusted `a`) enrolling `b`, pushed but not yet stored.
	let objects = trust_commit(&a, &[&a, &b], "require", vec![bootstrap]);
	let new_tip = commit_id_of(&objects);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/gitana/trust", Some(bootstrap), new_tip)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn off_still_hard_rejects_trust_ref_deletion() {
	let a = key(1);
	let repo = new_repo().await;
	// Even under `off`, the trust root's own integrity is enforced: it cannot be deleted.
	let tip = install_root(&repo, &a, &[&a], "off").await;
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_delete("refs/gitana/trust", tip)],
		&Objects::new(),
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/gitana/trust"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn warn_still_hard_rejects_an_invalid_trust_update() {
	let (a, b) = (key(1), key(2));
	let repo = new_repo().await;
	// Current root is `warn`, enrolling only `a`.
	let bootstrap = install_root(&repo, &a, &[&a], "warn").await;
	// A trust update signed by the untrusted `b`: warn must NOT let it poison the trust root.
	let objects = trust_commit(&b, &[&a, &b], "warn", vec![bootstrap]);
	let new_tip = commit_id_of(&objects);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/gitana/trust", Some(bootstrap), new_tip)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/gitana/trust"),
				"{refs:?}"
			);
		}
		other => panic!("expected hard reject under warn, got {other:?}"),
	}
}

#[tokio::test]
async fn accepts_a_valid_bootstrap_creation() {
	let a = key(1);
	let repo = new_repo().await; // no trust ref yet
	// A self-signed bootstrap (signed by `a`, which the doc enrols), pushed to create the trust ref.
	let objects = trust_commit(&a, &[&a], "require", vec![]);
	let tip = commit_id_of(&objects);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/gitana/trust", None, tip)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn rejects_a_bootstrap_not_self_signed() {
	let (a, b) = (key(1), key(2));
	let repo = new_repo().await;
	// The bootstrap doc enrols `a` but the commit is signed by `b`: not self-signed.
	let objects = trust_commit(&b, &[&a], "require", vec![]);
	let tip = commit_id_of(&objects);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/gitana/trust", None, tip)],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/gitana/trust"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn rejects_trust_ref_deletion() {
	let a = key(1);
	let repo = new_repo().await;
	let tip = install_root(&repo, &a, &[&a], "require").await;
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_delete("refs/gitana/trust", tip)],
		&HashMap::new(),
		None,
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/gitana/trust"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn require_rejects_moving_a_protected_ref_to_an_unsigned_stored_commit() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// An unsigned commit already in the store (as if first pushed to an unprotected ref). It is not
	// reachable from any protected ref, so it is NOT grandfathered.
	let (stored, raw) = commit_bytes(None, "unsigned but stored\n");
	repo
		.objects()
		.write_object(ObjectKind::Commit, &raw)
		.await
		.expect("store");
	// A signed push moving a protected branch onto it (empty pack — the object is already stored).
	let cert = signed_cert(
		&a,
		vec![cert_command(stored, "refs/heads/main")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, stored)],
		&Objects::new(),
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/heads/main"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn rejects_a_mixed_bootstrap_and_protected_push() {
	let a = key(1);
	let repo = new_repo().await; // no trust ref
	// The push both creates the trust ref and updates a protected branch — refused; bootstrap alone.
	let boot = trust_commit(&a, &[&a], "require", vec![]);
	let boot_tip = commit_id_of(&boot);
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let mut objects = boot;
	objects.insert(commit, (ObjectKind::Commit, raw));
	objects.insert(empty_tree(), (ObjectKind::Tree, encode_tree::<Sha256>(&[])));
	let verdict = verify_push(
		&repo,
		&context(),
		&[
			ref_update("refs/gitana/trust", None, boot_tip),
			ref_update("refs/heads/main", None, commit),
		],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert!(matches!(
		verdict,
		TrustVerdict::Reject {
			global: Some(_),
			..
		}
	));
}

#[tokio::test]
async fn rejects_a_trust_policy_change_mixed_with_protected_refs() {
	let a = key(1);
	let repo = new_repo().await;
	// Current root is lenient (warn); the push flips it to require AND moves a protected ref, which
	// would otherwise be judged under the old warn policy but land under the new require root.
	let bootstrap = install_root(&repo, &a, &[&a], "warn").await;
	let trust = trust_commit(&a, &[&a], "require", vec![bootstrap]);
	let trust_tip = commit_id_of(&trust);
	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let mut objects = trust;
	objects.insert(commit, (ObjectKind::Commit, raw));
	objects.insert(empty_tree(), (ObjectKind::Tree, encode_tree::<Sha256>(&[])));
	let verdict = verify_push(
		&repo,
		&context(),
		&[
			ref_update("refs/gitana/trust", Some(bootstrap), trust_tip),
			ref_update("refs/heads/main", None, commit),
		],
		&objects,
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert!(matches!(
		verdict,
		TrustVerdict::Reject {
			global: Some(_),
			..
		}
	));
}

#[tokio::test]
async fn require_rejects_a_protected_branch_pointing_at_a_non_commit() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// Point a protected branch straight at a tree object.
	let tree = empty_tree();
	let objects: Objects = [(tree, (ObjectKind::Tree, encode_tree::<Sha256>(&[])))].into();
	let cert = signed_cert(
		&a,
		vec![cert_command(tree, "refs/heads/main")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, tree)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/heads/main"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn require_rejects_a_lightweight_protected_tag() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// The commit is signed, but a tag ref pointing straight at it is a lightweight tag.
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/tags/v1")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/tags/v1", None, commit)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/tags/v1"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn require_accepts_a_signed_annotated_protected_tag() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	let (commit, craw) = commit_bytes(Some(&a), "signed\n");
	let (tag, traw) = signed_tag(&a, commit, "v1");
	let mut objects = one_commit(commit, craw);
	objects.insert(tag, (ObjectKind::Tag, traw));
	let cert = signed_cert(&a, vec![cert_command(tag, "refs/tags/v1")], &fresh_nonce());
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/tags/v1", None, tag)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn require_rejects_protected_deletion_without_cert() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// Deleting a protected branch needs a trusted push certificate, just like an update.
	let old = commit_bytes(Some(&a), "x\n").0;
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_delete("refs/heads/main", old)],
		&HashMap::new(),
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert!(matches!(
		verdict,
		TrustVerdict::Reject {
			global: Some(_),
			..
		}
	));
}

#[tokio::test]
async fn require_accepts_a_signed_protected_deletion() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// A protected deletion carrying a valid certificate is authorised (the positive counterpart of the
	// no-cert rejection above). A delete sends no objects, so there is no signed-object walk.
	let old = commit_bytes(Some(&a), "x\n").0;
	let cert = signed_cert(
		&a,
		vec![CertCommand {
			old: old.to_hex(),
			new: ZERO.to_owned(),
			refname: "refs/heads/main".to_owned(),
		}],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_delete("refs/heads/main", old)],
		&HashMap::new(),
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	assert_eq!(verdict, TrustVerdict::Accept { warnings: vec![] });
}

#[tokio::test]
async fn require_rejects_a_cert_signed_by_an_untrusted_key() {
	let (a, b) = (key(1), key(2));
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// The commit is signed by the trusted `a`, but the push certificate is signed by `b`, who is not
	// enrolled in the root. The object signatures pass; the certificate signature does not — so the
	// whole push is rejected globally (a bad cert fails every ref).
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&b,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	assert!(
		matches!(
			verdict,
			TrustVerdict::Reject {
				global: Some(_),
				..
			}
		),
		"{verdict:?}"
	);
}

#[tokio::test]
async fn require_rejects_an_unsigned_annotated_protected_tag() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// A real annotated tag object (so it clears the "must be a tag object" check that a lightweight
	// tag fails) pointing at a signed commit — but the tag itself carries no signature.
	let (commit, craw) = commit_bytes(Some(&a), "signed\n");
	let (tag, traw) = unsigned_tag(commit, "v1");
	let mut objects = one_commit(commit, craw);
	objects.insert(tag, (ObjectKind::Tag, traw));
	let cert = signed_cert(&a, vec![cert_command(tag, "refs/tags/v1")], &fresh_nonce());
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/tags/v1", None, tag)],
		&objects,
		Some(&cert),
		NOW,
	)
	.await
	.expect("verify");
	match verdict {
		TrustVerdict::Reject { refs, .. } => {
			assert!(
				refs.iter().any(|(name, _)| name == "refs/tags/v1"),
				"{refs:?}"
			);
		}
		other => panic!("expected reject, got {other:?}"),
	}
}

#[tokio::test]
async fn fails_closed_when_the_current_trust_root_is_unverifiable() {
	let a = key(1);
	let repo = new_repo().await;
	// Point `refs/gitana/trust` at an ordinary commit (empty tree, no `trust.json`): a root that
	// exists but cannot be folded. Protected writes must fail *closed* — a corrupt trust anchor must
	// never silently disable enforcement.
	let (bad, raw) = commit_bytes(Some(&a), "not a trust commit\n");
	repo
		.objects()
		.write_object(ObjectKind::Tree, &encode_tree::<Sha256>(&[]))
		.await
		.expect("write tree");
	repo
		.objects()
		.write_object(ObjectKind::Commit, &raw)
		.await
		.expect("write commit");
	repo
		.refs()
		.update_ref("refs/gitana/trust", bad, None, ReflogIntent::Skip)
		.await
		.expect("set trust ref");

	// Any protected push is refused before policy even runs; the fold failure is a whole-push reject.
	let (commit, craw) = commit_bytes(Some(&a), "signed\n");
	let verdict = verify_push(
		&repo,
		&context(),
		&[ref_update("refs/heads/main", None, commit)],
		&one_commit(commit, craw),
		None,
		NOW,
	)
	.await
	.expect("verify");
	assert!(
		matches!(
			verdict,
			TrustVerdict::Reject {
				global: Some(_),
				..
			}
		),
		"{verdict:?}"
	);
}

// --- receive_pack wiring -----------------------------------------------------------------------
//
// These drive the same enforcement through the wire path (`receive_pack`) instead of `verify_push`
// directly, so they cover what the pure core cannot: that a verdict is rendered into report-status
// and that refs actually move (accept) or stay put (reject).

/// Encode `objects` into a packfile — the pushed objects a `receive_pack` request carries.
fn pack_of(objects: &Objects) -> Vec<u8> {
	let packed: Vec<PackedObject<Sha256>> = objects
		.iter()
		.map(|(id, (kind, data))| PackedObject {
			id: *id,
			kind: *kind,
			data: data.clone(),
		})
		.collect();
	encode_pack(&packed)
}

/// A plain (unsigned) receive-pack request: one `<old> <new> <ref>` command line (caps on it) +
/// flush + the pack of `objects`.
fn plain_request(objects: &Objects, old: Option<Oid>, new: Oid, name: &str) -> Vec<u8> {
	let old = old.map_or_else(|| ZERO.to_owned(), |o| o.to_hex());
	let command = format!(
		"{old} {} {name}\0report-status object-format=sha256\n",
		new.to_hex()
	);
	let mut out = Vec::new();
	out.extend_from_slice(format!("{:04x}{command}", command.len() + 4).as_bytes());
	out.extend_from_slice(b"0000");
	out.extend_from_slice(&pack_of(objects));
	out
}

/// Run `receive_pack` with the test trust context at `NOW`, force off.
async fn run_receive(repo: &Repo, request: &[u8]) -> ReceiveOutcome<Sha256> {
	receive_pack(
		repo,
		request,
		ReceiveOptions {
			force: false,
			trust: &context(),
			now: NOW,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive")
}

fn report_text(outcome: &ReceiveOutcome<Sha256>) -> String {
	String::from_utf8_lossy(&outcome.report).into_owned()
}

#[tokio::test]
async fn wire_require_rejects_unsigned_push_and_leaves_ref_unmoved() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;

	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let request = plain_request(&objects, None, commit, "refs/heads/main");

	let outcome = run_receive(&repo, &request).await;
	let text = report_text(&outcome);
	assert!(text.contains("unpack ok"), "{text}");
	assert!(text.contains("ng refs/heads/main"), "{text}");
	// A whole-push (global) rejection moves nothing and writes nothing.
	assert!(outcome.updated.is_empty());
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		None
	);
	assert!(
		!repo.objects().exists_object(&commit).await.expect("exists"),
		"a globally rejected push writes no objects"
	);
	// This push is rejected on two counts at once — no push certificate (whole-push) and an unsigned
	// commit under a protected ref (per-ref) — and the audit trail keeps both, even though the wire
	// report is a blanket rejection.
	assert!(
		outcome
			.audit
			.iter()
			.any(|event| matches!(event, AuditEvent::PushRejected { .. })),
		"{:?}",
		outcome.audit
	);
	assert!(
		outcome.audit.iter().any(|event| matches!(
			event,
			AuditEvent::RefRejected { name, .. } if name == "refs/heads/main"
		)),
		"{:?}",
		outcome.audit
	);
}

#[tokio::test]
async fn wire_require_accepts_valid_signed_push_and_moves_ref() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;

	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let request = build_push_cert(
		&cert,
		"report-status object-format=sha256",
		&pack_of(&objects),
	);

	let outcome = run_receive(&repo, &request).await;
	let text = report_text(&outcome);
	assert!(text.contains("ok refs/heads/main"), "{text}");
	assert_eq!(
		outcome.updated,
		vec![("refs/heads/main".to_owned(), commit)]
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(commit)
	);
	// The audit trail records an acceptance with no warnings.
	assert_eq!(
		outcome.audit,
		vec![AuditEvent::PushAccepted {
			refs: vec!["refs/heads/main".to_owned()],
			warnings: Vec::new(),
		}]
	);
}

#[tokio::test]
async fn wire_require_applies_a_signed_delete_when_the_host_grants_deletes() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;
	// A protected branch exists (a signed commit).
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	repo
		.objects()
		.write_object(ObjectKind::Commit, &raw)
		.await
		.expect("write commit");
	repo
		.objects()
		.write_object(ObjectKind::Tree, &encode_tree::<Sha256>(&[]))
		.await
		.expect("write tree");
	repo
		.refs()
		.update_ref("refs/heads/main", commit, None, ReflogIntent::Skip)
		.await
		.expect("set ref");

	// A signed delete certificate for it (new value zeroed), carrying no pack.
	let cert = signed_cert(
		&a,
		vec![CertCommand {
			old: commit.to_hex(),
			new: ZERO.to_owned(),
			refname: "refs/heads/main".to_owned(),
		}],
		&fresh_nonce(),
	);
	let request = build_push_cert(&cert, "report-status object-format=sha256", &[]);

	// Trust authorises *who* deletes; the host's delete grant (`force`, git's delete-refs capability)
	// authorises deletes *at all* — orthogonal axes. With the grant, the signed, trusted delete lands.
	let outcome = receive_pack(
		&repo,
		&request,
		ReceiveOptions {
			force: true,
			trust: &context(),
			now: NOW,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive");
	let text = report_text(&outcome);
	assert!(text.contains("ok refs/heads/main"), "{text}");
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		None,
		"the protected ref was deleted"
	);
}

#[tokio::test]
async fn wire_warn_surfaces_warnings_and_moves_ref() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "warn").await;

	let (commit, raw) = commit_bytes(None, "unsigned\n");
	let objects = one_commit(commit, raw);
	let request = plain_request(&objects, None, commit, "refs/heads/main");

	let outcome = run_receive(&repo, &request).await;
	let text = report_text(&outcome);
	assert!(text.contains("ok refs/heads/main"), "{text}");
	// Under warn the failures are recorded on the audit trail (for a host to log) but not enforced:
	// the push is accepted, carrying the unenforced failures as warnings.
	match outcome.audit.as_slice() {
		[AuditEvent::PushAccepted { refs, warnings }] => {
			assert_eq!(refs, &["refs/heads/main".to_owned()]);
			assert!(!warnings.is_empty(), "warn should record the failures");
		}
		other => panic!("expected accept-with-warnings, got {other:?}"),
	}
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(commit)
	);
}

#[tokio::test]
async fn wire_require_partial_reject_applies_good_ref_and_ngs_bad() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;

	// A valid cert covering both refs, but only one commit is signed: the signed ref lands, the
	// unsigned ref is `ng`'d — a per-ref (non-global) rejection.
	let (good, good_raw) = commit_bytes(Some(&a), "good\n");
	let (bad, bad_raw) = commit_bytes(None, "bad\n");
	let mut objects = Objects::new();
	objects.insert(empty_tree(), (ObjectKind::Tree, encode_tree::<Sha256>(&[])));
	objects.insert(good, (ObjectKind::Commit, good_raw));
	objects.insert(bad, (ObjectKind::Commit, bad_raw));
	let cert = signed_cert(
		&a,
		vec![
			cert_command(good, "refs/heads/good"),
			cert_command(bad, "refs/heads/bad"),
		],
		&fresh_nonce(),
	);
	let request = build_push_cert(
		&cert,
		"report-status object-format=sha256",
		&pack_of(&objects),
	);

	let outcome = run_receive(&repo, &request).await;
	let text = report_text(&outcome);
	assert!(text.contains("ok refs/heads/good"), "{text}");
	assert!(text.contains("ng refs/heads/bad"), "{text}");
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/good")
			.await
			.expect("resolve"),
		Some(good),
		"the signed ref lands"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/bad")
			.await
			.expect("resolve"),
		None,
		"the unsigned ref is rejected"
	);
	// The rejected ref's object must not have been migrated out of quarantine…
	assert!(
		!repo.objects().exists_object(&bad).await.expect("exists"),
		"a trust-rejected ref must not persist its objects"
	);
	// …while the accepted ref's object is stored.
	assert!(
		repo.objects().exists_object(&good).await.expect("exists"),
		"the accepted ref's objects are migrated"
	);
	// The audit trail records the per-ref rejection and, separately, the accepted ref.
	assert!(
		outcome.audit.iter().any(|event| matches!(
			event,
			AuditEvent::RefRejected { name, .. } if name == "refs/heads/bad"
		)),
		"{:?}",
		outcome.audit
	);
	assert!(
		outcome.audit.contains(&AuditEvent::PushAccepted {
			refs: vec!["refs/heads/good".to_owned()],
			warnings: Vec::new(),
		}),
		"{:?}",
		outcome.audit
	);
}

#[tokio::test]
async fn wire_trust_cleared_but_non_fast_forward_ref_is_not_audited_as_accepted() {
	// A repo with no trust root: verify_push accepts everything, so acceptance is decided entirely
	// by the receive path. The audit must reflect what actually moved, not what trust cleared.
	let repo = new_repo().await;

	let (first, first_raw) = commit_bytes(None, "one\n");
	let create = plain_request(
		&one_commit(first, first_raw),
		None,
		first,
		"refs/heads/main",
	);
	let created = run_receive(&repo, &create).await;
	assert_eq!(
		created.audit,
		vec![AuditEvent::PushAccepted {
			refs: vec!["refs/heads/main".to_owned()],
			warnings: Vec::new(),
		}]
	);

	// An unrelated commit is not a fast-forward of the first; with force off the receive path `ng`s
	// it even though trust raised no objection.
	let (second, second_raw) = commit_bytes(None, "two\n");
	let update = plain_request(
		&one_commit(second, second_raw),
		Some(first),
		second,
		"refs/heads/main",
	);
	let outcome = run_receive(&repo, &update).await;
	let text = report_text(&outcome);
	assert!(
		text.contains("ng refs/heads/main non-fast-forward"),
		"{text}"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(first),
		"the ref did not move"
	);
	// The audit records no acceptance — nothing landed and there was nothing to warn about.
	assert!(
		outcome.audit.is_empty(),
		"a non-fast-forward ref must not be audited as accepted: {:?}",
		outcome.audit
	);
}

#[tokio::test]
async fn wire_none_context_fails_closed_on_a_trust_configured_repo() {
	let a = key(1);
	let repo = new_repo().await;
	install_root(&repo, &a, &[&a], "require").await;

	// A push validly signed by a trusted key, but received with an empty (`none`) trust context.
	// The certificate cannot be freshness/binding-checked, so the push fails closed.
	let (commit, raw) = commit_bytes(Some(&a), "signed\n");
	let objects = one_commit(commit, raw);
	let cert = signed_cert(
		&a,
		vec![cert_command(commit, "refs/heads/main")],
		&fresh_nonce(),
	);
	let request = build_push_cert(
		&cert,
		"report-status object-format=sha256",
		&pack_of(&objects),
	);

	let outcome = receive_pack(
		&repo,
		&request,
		ReceiveOptions {
			force: false,
			trust: &TrustContext::none(),
			now: NOW,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive");
	let text = report_text(&outcome);
	assert!(text.contains("ng refs/heads/main"), "{text}");
	assert!(outcome.updated.is_empty());
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		None,
		"a none context fails a protected push closed"
	);
	assert!(
		!repo.objects().exists_object(&commit).await.expect("exists"),
		"a rejected push writes no objects"
	);
}

#[tokio::test]
async fn wire_bootstrap_installs_trust_root() {
	let a = key(1);
	let repo = new_repo().await;

	// A self-signed bootstrap creating refs/gitana/trust is accepted and the ref moves.
	let objects = trust_commit(&a, &[&a], "require", vec![]);
	let tip = commit_id_of(&objects);
	let request = plain_request(&objects, None, tip, "refs/gitana/trust");

	let outcome = run_receive(&repo, &request).await;
	let text = report_text(&outcome);
	assert!(text.contains("ok refs/gitana/trust"), "{text}");
	assert_eq!(
		repo
			.refs()
			.resolve("refs/gitana/trust")
			.await
			.expect("resolve"),
		Some(tip)
	);
}
