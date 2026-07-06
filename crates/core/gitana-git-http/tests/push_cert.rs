//! Push-certificate codec + nonce: the signed-payload bytes match git's format (asserted against a
//! real `git push --signed` certificate as well as gitana's own round-trip), a cert round-trips
//! through build→parse, the nonce HMAC accepts only fresh untampered nonces, and a signed push moves
//! the ref while surfacing the certificate for the host to verify.

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{
	CertCommand, NoReplayCheck, PushCert, ReceiveOptions, TrustContext, build_push_cert, make_nonce,
	receive_pack, verify_nonce,
};
use gitana_object::Sha256;
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, TreeEntry, encode_commit, encode_pack, encode_tree,
};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_trust::{TrustedKey, verify_sshsig};

const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A real push certificate captured from stock `git push --signed` (git 2.50.1, `gpg.format=ssh`)
/// and the ed25519 key that signed it — so the test below verifies gitana against git's *actual*
/// output rather than its own round-trip.
const REAL_GIT_CERT: &str = include_str!("fixtures/real_git_push_cert.txt");
const REAL_GIT_KEY: &str = include_str!("fixtures/real_git_push_cert.pub");

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

/// Reconstruct a [`PushCert`] from the body git actually signs (everything up to the signature
/// marker). Mirrors the header/blank/commands layout of git's certificate.
fn cert_from_git_payload(payload: &str, signature: &str) -> PushCert {
	let mut lines = payload.lines();
	let version = lines
		.next()
		.and_then(|l| l.strip_prefix("certificate version "))
		.expect("certificate version header")
		.to_owned();
	let (mut pusher, mut pushee, mut nonce) = (String::new(), String::new(), String::new());
	for line in lines.by_ref() {
		if line.is_empty() {
			break; // the blank line separating headers from commands
		} else if let Some(rest) = line.strip_prefix("pusher ") {
			pusher = rest.to_owned();
		} else if let Some(rest) = line.strip_prefix("pushee ") {
			pushee = rest.to_owned();
		} else if let Some(rest) = line.strip_prefix("nonce ") {
			nonce = rest.to_owned();
		}
	}
	let commands = lines
		.map(|line| {
			let mut parts = line.splitn(3, ' ');
			CertCommand {
				old: parts.next().unwrap_or_default().to_owned(),
				new: parts.next().unwrap_or_default().to_owned(),
				refname: parts.next().unwrap_or_default().to_owned(),
			}
		})
		.collect();
	PushCert {
		version,
		pusher,
		pushee,
		nonce,
		push_options: Vec::new(),
		commands,
		signature: signature.to_owned(),
	}
}

/// The step-8 anchor test: gitana verifies a certificate produced by *real* `git push --signed`
/// (not gitana's own round-trip). Confirms two things the enforcement path assumes — that
/// `PushCert::payload()` reproduces git's signed bytes exactly, and that the SSHSIG namespace git
/// uses for push certificates is `"git"` (the same as commits/tags). Verified empirically against
/// git 2.50.1; this locks it against regression.
#[test]
fn verifies_a_real_git_push_certificate() {
	let marker = "-----BEGIN SSH SIGNATURE-----";
	let (payload, rest) = REAL_GIT_CERT
		.split_once(marker)
		.expect("fixture carries a signature block");
	let armor = format!("{marker}{rest}");
	let armor = armor.trim_end();

	let cert = cert_from_git_payload(payload, armor);

	// `payload()` must reproduce, byte for byte, exactly what git signed — otherwise verification of a
	// real signed push would fail on the wire.
	assert_eq!(
		cert.payload(),
		payload.as_bytes(),
		"PushCert::payload() diverged from the bytes git signs"
	);

	// And git's real signature verifies over those bytes in the `"git"` SSHSIG namespace — the
	// assumption `verify_cert` rests on, now confirmed against stock git.
	let key = TrustedKey::from_openssh(REAL_GIT_KEY.trim()).expect("fixture signing key");
	let signed = verify_sshsig(
		&cert.payload(),
		armor.as_bytes(),
		std::slice::from_ref(&key),
		"git",
	)
	.expect("real git push-cert verifies in namespace=git");
	assert_eq!(signed, key.id());
}

#[test]
fn nonce_accepts_fresh_untampered_and_rejects_otherwise() {
	let secret = b"server-secret";
	let repo = "acme/app";
	let now = 1_700_000_000;
	let nonce = make_nonce(secret, repo, now, b"\x01\x02\x03\x04");

	// Fresh, right repo, within the slop window: accepted.
	assert!(verify_nonce(secret, repo, &nonce, now + 60, 900));
	// Outside the slop window: rejected (replay protection).
	assert!(!verify_nonce(secret, repo, &nonce, now + 5_000, 900));
	// A different repository: rejected — a cert for repo A cannot be replayed to repo B.
	assert!(!verify_nonce(secret, "acme/other", &nonce, now + 60, 900));
	// Tampered HMAC: rejected.
	let tampered = format!("{now}-01020304-deadbeef");
	assert!(!verify_nonce(secret, repo, &tampered, now, 900));
	// Wrong secret: rejected.
	assert!(!verify_nonce(b"other-secret", repo, &nonce, now, 900));
	// Garbage: rejected, no panic.
	assert!(!verify_nonce(secret, repo, "not-a-nonce", now, 900));
}

#[test]
fn nonce_is_unique_per_random() {
	let secret = b"server-secret";
	let repo = "acme/app";
	let now = 1_700_000_000;
	let a = make_nonce(secret, repo, now, b"\x00\x00\x00\x01");
	let b = make_nonce(secret, repo, now, b"\x00\x00\x00\x02");
	assert_ne!(a, b, "different random bytes yield different nonces");
	assert!(verify_nonce(secret, repo, &a, now, 900));
	assert!(verify_nonce(secret, repo, &b, now, 900));
}

// --- integration: a signed push through receive_pack ------------------------------

fn repo() -> Repository<MemoryFileStore, Sha256> {
	Repository::new(ObjectStore::<_, Sha256>::new(MemoryFileStore::new()))
}

/// Build a blob+tree+commit set and return it with the commit id.
fn commit_objects(content: &[u8]) -> (Vec<PackedObject<Sha256>>, ObjectId<Sha256>) {
	let blob = content.to_vec();
	let blob_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &blob);
	let tree = encode_tree(&[TreeEntry {
		mode: "100644".to_owned(),
		name: "file.txt".to_owned(),
		id: blob_id,
	}]);
	let tree_id = ObjectId::<Sha256>::compute(ObjectKind::Tree, &tree);
	let commit = encode_commit(&Commit {
		tree: tree_id,
		parents: vec![],
		author: "A <a@x> 1 +0000".to_owned(),
		committer: "A <a@x> 1 +0000".to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: "root\n".to_owned(),
	});
	let commit_id = ObjectId::<Sha256>::compute(ObjectKind::Commit, &commit);
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
	let nonce = make_nonce(b"secret", "acme/app", 1_700_000_000, b"\x01\x02\x03\x04");
	let original = cert(
		&nonce,
		vec![CertCommand {
			old: ZERO.to_owned(),
			new: commit.to_hex(),
			refname: "refs/heads/main".to_owned(),
		}],
	);
	let request = build_push_cert(&original, "report-status object-format=sha256", &pack);

	// No trust root is configured here, so the certificate is surfaced but not enforced.
	let outcome = receive_pack(
		&repo,
		&request,
		ReceiveOptions {
			force: false,
			trust: &TrustContext::none(),
			now: 0,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive");

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
	assert!(verify_nonce(
		b"secret",
		"acme/app",
		&surfaced.nonce,
		1_700_000_000,
		900
	));
}
