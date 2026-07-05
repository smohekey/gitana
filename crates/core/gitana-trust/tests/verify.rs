//! Verify against real stock-git fixtures: `git commit -S` / `git tag -s` objects produced under
//! `gpg.format=ssh` with captured keys (see `tests/fixtures/`). Verification takes the raw object
//! bytes, so it reproduces exactly what git signed.

use gitana_object::{Sha1, parse_commit};
use gitana_trust::{TrustError, TrustedKey, verify_commit, verify_sshsig, verify_tag};

/// The key that signed the ed25519 commit/tag fixtures.
const SIGNER_PUB: &str = include_str!("fixtures/signer.pub");
/// A different key that signed nothing here.
const OTHER_PUB: &str = include_str!("fixtures/other.pub");
const SIGNED_COMMIT: &[u8] = include_bytes!("fixtures/signed_commit.obj");
const SIGNED_TAG: &[u8] = include_bytes!("fixtures/signed_tag.obj");
/// The signer's SHA-256 fingerprint, as `ssh-keygen -lf` / `git` print it.
const SIGNER_FINGERPRINT: &str = "SHA256:8rQT7qQXoP52gfbVe93AMgGBOJeEDmR5il4Sj/mmxG0";

fn signer() -> TrustedKey {
	TrustedKey::from_openssh(SIGNER_PUB).expect("parse signer key")
}

fn other() -> TrustedKey {
	TrustedKey::from_openssh(OTHER_PUB).expect("parse other key")
}

#[test]
fn trusted_key_id_is_its_fingerprint() {
	assert_eq!(signer().id().as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_a_stock_git_signed_commit() {
	let key = verify_commit::<Sha1>(SIGNED_COMMIT, &[signer()]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_a_stock_git_signed_tag() {
	let key = verify_tag(SIGNED_TAG, &[signer()]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_signatures_over_non_utf8_messages() {
	// git signs the raw object bytes; a latin1 (non-UTF-8) message must not block verification.
	let keys = [TrustedKey::from_openssh(include_str!("fixtures/latin1_signer.pub")).expect("key")];
	verify_commit::<Sha1>(include_bytes!("fixtures/latin1_commit.obj"), &keys)
		.expect("verify latin1 commit");
	verify_tag(include_bytes!("fixtures/latin1_tag.obj"), &keys).expect("verify latin1 tag");
}

#[test]
fn verifies_stock_git_signatures_from_rsa_and_ecdsa_keys() {
	// git signs with ed25519, rsa, or ecdsa SSH keys; all must verify, not just ed25519.
	for (obj, pubkey) in [
		(
			include_bytes!("fixtures/signed_commit_rsa.obj").as_slice(),
			include_str!("fixtures/rsa.pub"),
		),
		(
			include_bytes!("fixtures/signed_commit_ecdsa.obj").as_slice(),
			include_str!("fixtures/ecdsa.pub"),
		),
	] {
		let key = TrustedKey::from_openssh(pubkey).expect("parse key");
		verify_commit::<Sha1>(obj, &[key]).expect("verify");
	}
}

#[test]
fn verifies_a_merge_of_a_signed_tag_with_a_mergetag_header() {
	// git signs the `mergetag` header too; verifying from the raw buffer (not a lossy re-encode)
	// reproduces those bytes, where the parsed-struct path would fail.
	let obj = include_bytes!("fixtures/mergetag_commit.obj").as_slice();
	let key = TrustedKey::from_openssh(include_str!("fixtures/mergetag_signer.pub")).expect("key");
	verify_commit::<Sha1>(obj, &[key]).expect("verify merge commit with mergetag");
}

#[test]
fn finds_the_trusted_key_among_several() {
	let key = verify_commit::<Sha1>(SIGNED_COMMIT, &[other(), signer()]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn rejects_a_signature_by_an_untrusted_key() {
	match verify_commit::<Sha1>(SIGNED_COMMIT, &[other()]) {
		Err(TrustError::UntrustedKey(id)) => assert_eq!(id.as_str(), SIGNER_FINGERPRINT),
		other => panic!("expected UntrustedKey, got {other:?}"),
	}
}

#[test]
fn rejects_a_tampered_payload() {
	// Re-encode the commit with a changed message: the signer key still matches, but the signed
	// bytes differ, so the signature must fail.
	let mut commit = parse_commit::<Sha1>(SIGNED_COMMIT).expect("parse");
	commit.message = "tampered\n".to_owned();
	let raw = gitana_object::encode_commit(&commit);
	assert!(matches!(
		verify_commit::<Sha1>(&raw, &[signer()]),
		Err(TrustError::BadSignature)
	));
}

#[test]
fn reports_an_unsigned_object() {
	let mut commit = parse_commit::<Sha1>(SIGNED_COMMIT).expect("parse");
	commit.signature = None;
	let raw = gitana_object::encode_commit(&commit);
	assert!(matches!(
		verify_commit::<Sha1>(&raw, &[signer()]),
		Err(TrustError::Unsigned)
	));
}

#[test]
fn reports_an_object_without_a_signature_as_unsigned() {
	// No parsing: bytes with no signature block are simply unsigned, not a hard error.
	assert!(matches!(
		verify_commit::<Sha1>(b"not a commit", &[signer()]),
		Err(TrustError::Unsigned)
	));
}

#[test]
fn rejects_a_malformed_signature_block() {
	let err =
		verify_sshsig(b"payload", b"not a signature", &[signer()], "git").expect_err("should reject");
	assert!(matches!(err, TrustError::MalformedSignature(_)), "{err:?}");
}

#[test]
fn rejects_a_malformed_public_key() {
	let err = TrustedKey::from_openssh("ssh-ed25519 not-base64").expect_err("should reject");
	assert!(matches!(err, TrustError::MalformedKey(_)), "{err:?}");
}
