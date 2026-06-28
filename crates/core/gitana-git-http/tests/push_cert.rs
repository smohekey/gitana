//! Push-certificate codec + nonce: the signed-payload bytes match git's format, a cert
//! round-trips through build→parse, the nonce HMAC accepts only fresh untampered nonces,
//! and a signed push moves the ref while surfacing the certificate for the host to verify.

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{
	CertCommand, PushCert, build_push_cert, make_nonce, receive_pack, verify_nonce,
};
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, TreeEntry, encode_commit, encode_pack, encode_tree,
};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;

const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A sample armored SSH signature (the codec treats it as opaque text).
const SIGNATURE: &str =
	"-----BEGIN SSH SIGNATURE-----\nAAAAfoobar\nbazqux==\n-----END SSH SIGNATURE-----\n";

fn cert(nonce: &str, commands: Vec<CertCommand>) -> PushCert {
	PushCert {
		version: "0.1".to_owned(),
		pusher: "Dev <dev@x.test> 1700000000 +0000".to_owned(),
		pushee: "http://host/acme/app".to_owned(),
		nonce: nonce.to_owned(),
		push_options: Vec::new(),
		commands,
		signature: SIGNATURE.to_owned(),
	}
}

#[test]
fn payload_matches_the_git_certificate_format() {
	let cert = cert(
		"1700000000-abc",
		vec![CertCommand {
			old: ZERO.to_owned(),
			new: "a".repeat(64),
			refname: "refs/heads/main".to_owned(),
		}],
	);
	let expected = format!(
		"certificate version 0.1\n\
		 pusher Dev <dev@x.test> 1700000000 +0000\n\
		 pushee http://host/acme/app\n\
		 nonce 1700000000-abc\n\
		 \n\
		 {zero} {ones} refs/heads/main\n",
		zero = ZERO,
		ones = "a".repeat(64),
	);
	assert_eq!(cert.payload(), expected.as_bytes());
}

#[test]
fn nonce_accepts_fresh_untampered_and_rejects_otherwise() {
	let secret = b"server-secret";
	let now = 1_700_000_000;
	let nonce = make_nonce(secret, now);

	// Fresh and within the slop window: accepted.
	assert!(verify_nonce(secret, &nonce, now + 60, 900));
	// Outside the slop window: rejected (replay protection).
	assert!(!verify_nonce(secret, &nonce, now + 5_000, 900));
	// Tampered HMAC: rejected.
	let tampered = format!("{now}-deadbeef");
	assert!(!verify_nonce(secret, &tampered, now, 900));
	// Wrong secret: rejected.
	assert!(!verify_nonce(b"other-secret", &nonce, now, 900));
	// Garbage: rejected, no panic.
	assert!(!verify_nonce(secret, "not-a-nonce", now, 900));
}

// --- integration: a signed push through receive_pack ------------------------------

fn repo() -> Repository<MemoryFileStore> {
	Repository::new(ObjectStore::new(MemoryFileStore::new()))
}

/// Build a blob+tree+commit set and return it with the commit id.
fn commit_objects(content: &[u8]) -> (Vec<PackedObject>, ObjectId) {
	let blob = content.to_vec();
	let blob_id = ObjectId::compute(ObjectKind::Blob, &blob);
	let tree = encode_tree(&[TreeEntry {
		mode: "100644".to_owned(),
		name: "file.txt".to_owned(),
		id: blob_id,
	}]);
	let tree_id = ObjectId::compute(ObjectKind::Tree, &tree);
	let commit = encode_commit(&Commit {
		tree: tree_id,
		parents: vec![],
		author: "A <a@x> 1 +0000".to_owned(),
		committer: "A <a@x> 1 +0000".to_owned(),
		signature: None,
		message: "root\n".to_owned(),
	});
	let commit_id = ObjectId::compute(ObjectKind::Commit, &commit);
	let objects = vec![
		PackedObject {
			id: blob_id,
			kind: ObjectKind::Blob,
			data: blob,
		},
		PackedObject {
			id: tree_id,
			kind: ObjectKind::Tree,
			data: tree,
		},
		PackedObject {
			id: commit_id,
			kind: ObjectKind::Commit,
			data: commit,
		},
	];
	(objects, commit_id)
}

#[tokio::test]
async fn signed_push_moves_ref_and_surfaces_cert() {
	let repo = repo();
	repo.init().await.expect("init");

	let (objects, commit) = commit_objects(b"hello\n");
	let pack = encode_pack(&objects);
	let nonce = make_nonce(b"secret", 1_700_000_000);
	let original = cert(
		&nonce,
		vec![CertCommand {
			old: ZERO.to_owned(),
			new: commit.to_hex(),
			refname: "refs/heads/main".to_owned(),
		}],
	);
	let request = build_push_cert(&original, "report-status object-format=sha256", &pack);

	let outcome = receive_pack(&repo, &request, false).await.expect("receive");

	// The ref moved (the cert's command was applied)…
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(commit)
	);
	// …and the certificate is surfaced intact for the host to verify.
	let surfaced = outcome.push_cert.expect("cert surfaced");
	assert_eq!(surfaced, original);
	assert!(verify_nonce(b"secret", &surfaced.nonce, 1_700_000_000, 900));
}
