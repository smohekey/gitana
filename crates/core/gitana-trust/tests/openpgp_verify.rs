//! Verify OpenPGP-signed git commit/tag objects. The fixtures are byte-exact `git`-format objects
//! (a `gpgsig` header for commits, an appended armor block for tags) whose detached OpenPGP
//! signatures were produced over exactly the bytes git signs.
//!
//! Two provenances: `*_pgp.*` / `pgp_*` fixtures are produced by the same library gitana verifies
//! with (rpgp), exercising component shapes rpgp can build (a signing subkey, a third-party
//! certification); `real_gpg_*` fixtures are captured from **stock `git commit -S` / `git tag -s`
//! under GnuPG** (`gpg (GnuPG) 2.5`, ed25519), regression-locking cross-implementation interop — the
//! real question, "does gitana verify what other people's `gpg` actually produces" (parity with the
//! stock-git SSHSIG fixtures in `verify.rs`).
//!
//! Coverage note: the *key-expiry*, *grant-not-yet-valid*, and *non-Binary signature type* rejection
//! paths (see `verify_pgpsig`) have no integration fixture — they cannot arise from honest signing (a
//! key valid *when it signs* is never expired by the clock-free model; `gpg`/rpgp refuse to sign with
//! an expired key or to emit a non-data signature), so they guard hand-crafted packets and rest on the
//! `expired_at` unit test + review. The *revocation* path IS exercised: `real_gpg_revoked_*` below is a
//! real `gpg`-revoked certificate.

use gitana_object::{Sha1, encode_commit, parse_commit};
use gitana_trust::{
	Policy, TrustDocument, TrustError, TrustRoot, TrustedKey, verify_commit, verify_pgpsig,
	verify_tag,
};

/// The OpenPGP certificate that signed the commit/tag fixtures.
const SIGNER_PUB: &str = include_str!("fixtures/pgp_signer.pub.asc");
/// A different OpenPGP certificate that signed nothing here.
const OTHER_PUB: &str = include_str!("fixtures/other_pgp.pub.asc");
/// An OpenSSH key (from the SSHSIG fixtures) — a PGP signature must not verify against it.
const SSH_PUB: &str = include_str!("fixtures/signer.pub");
const SIGNED_COMMIT: &[u8] = include_bytes!("fixtures/signed_commit_pgp.obj");
const SIGNED_TAG: &[u8] = include_bytes!("fixtures/signed_tag_pgp.obj");
/// A certificate whose primary key is certify-only and whose signing *subkey* signed a commit.
const SUBKEY_SIGNER_PUB: &str = include_str!("fixtures/pgp_subkey_signer.pub.asc");
const SUBKEY_SIGNED_COMMIT: &[u8] = include_bytes!("fixtures/signed_commit_pgp_subkey.obj");
/// The signer certificate augmented with a third-party certification (by `other`) on its User ID —
/// as `gpg --export` includes once someone signs your key. It must still enrol and verify.
const THIRDPARTY_PUB: &str = include_str!("fixtures/pgp_signer_thirdparty.pub.asc");
/// A certificate captured from stock GnuPG, with the `git commit -S` / `git tag -s` objects it signed.
const REAL_GPG_PUB: &str = include_str!("fixtures/real_gpg_signer.pub.asc");
const REAL_GPG_COMMIT: &[u8] = include_bytes!("fixtures/real_gpg_signed_commit.obj");
const REAL_GPG_TAG: &[u8] = include_bytes!("fixtures/real_gpg_signed_tag.obj");
/// The stock-gpg signer's fingerprint (uppercase hex, as gitana's `KeyId` renders it).
const REAL_GPG_FINGERPRINT: &str = "852E9A7ACA98EC3F64530C4EEB339D265A192C29";
/// A stock-gpg certificate that was **revoked** (reason unspecified → retroactive), with a commit it
/// signed *before* the revocation. gitana must refuse the signature.
const REAL_GPG_REVOKED_PUB: &str = include_str!("fixtures/real_gpg_revoked_signer.pub.asc");
const REAL_GPG_REVOKED_COMMIT: &[u8] = include_bytes!("fixtures/real_gpg_revoked_commit.obj");
/// The signer certificate's OpenPGP fingerprint, uppercase hex (as `gpg` prints, ungrouped).
const SIGNER_FINGERPRINT: &str = "15C4BD0E22EB623FFAE8D39B97491FAA6FB8F8DB";
/// The subkey-signer certificate's *primary* fingerprint — verification attributes a subkey
/// signature to the enrolling certificate, not the subkey.
const SUBKEY_SIGNER_FINGERPRINT: &str = "E577687728BDAF535C7ED11F23869A0414D902E7";

fn signer() -> TrustedKey {
	TrustedKey::from_armored_pgp(SIGNER_PUB).expect("parse signer certificate")
}

fn other() -> TrustedKey {
	TrustedKey::from_armored_pgp(OTHER_PUB).expect("parse other certificate")
}

#[test]
fn parse_dispatches_on_armor() {
	assert!(matches!(
		TrustedKey::parse(SIGNER_PUB),
		Ok(TrustedKey::Pgp(_))
	));
	assert!(matches!(TrustedKey::parse(SSH_PUB), Ok(TrustedKey::Ssh(_))));
}

#[test]
fn pgp_key_id_is_its_fingerprint() {
	assert_eq!(signer().id().as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_a_stock_gpg_signed_commit() {
	// The real interop check: a `git commit -S` object produced by stock GnuPG (not rpgp) verifies.
	let cert = TrustedKey::from_armored_pgp(REAL_GPG_PUB).expect("parse gpg certificate");
	let key = verify_commit::<Sha1>(REAL_GPG_COMMIT, &[cert]).expect("verify");
	assert_eq!(key.as_str(), REAL_GPG_FINGERPRINT);
}

#[test]
fn verifies_a_stock_gpg_signed_tag() {
	let cert = TrustedKey::from_armored_pgp(REAL_GPG_PUB).expect("parse gpg certificate");
	let key = verify_tag(REAL_GPG_TAG, &[cert]).expect("verify");
	assert_eq!(key.as_str(), REAL_GPG_FINGERPRINT);
}

#[test]
fn rejects_a_signature_by_a_stock_gpg_revoked_key() {
	// A real gpg-revoked certificate (reason unspecified, which is retroactive). Even though this
	// commit was signed before the revocation, gitana must refuse it — a revoked key is not trusted.
	let cert = TrustedKey::from_armored_pgp(REAL_GPG_REVOKED_PUB).expect("parse revoked certificate");
	assert!(matches!(
		verify_commit::<Sha1>(REAL_GPG_REVOKED_COMMIT, &[cert]),
		Err(TrustError::BadSignature)
	));
}

#[test]
fn verifies_a_pgp_signed_commit() {
	let key = verify_commit::<Sha1>(SIGNED_COMMIT, &[signer()]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_a_pgp_signed_tag() {
	let key = verify_tag(SIGNED_TAG, &[signer()]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn finds_the_trusted_certificate_among_several() {
	// A mixed set — an unrelated PGP cert and an SSH key alongside the signer — still resolves.
	let keys = [
		other(),
		TrustedKey::from_openssh(SSH_PUB).expect("ssh key"),
		signer(),
	];
	let key = verify_commit::<Sha1>(SIGNED_COMMIT, &keys).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn enrols_and_verifies_a_certificate_carrying_third_party_certifications() {
	// A key exported after someone else signed it carries third-party User ID certifications that do
	// not verify against the primary. Validation is scoped to the *used* component, so the certificate
	// still parses (enrols) and its own signatures still verify — it is not rejected wholesale.
	let cert = TrustedKey::from_armored_pgp(THIRDPARTY_PUB).expect("enrol third-party-signed cert");
	assert_eq!(cert.id().as_str(), SIGNER_FINGERPRINT);
	let key = verify_commit::<Sha1>(SIGNED_COMMIT, &[cert]).expect("verify");
	assert_eq!(key.as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn verifies_a_commit_signed_by_a_signing_subkey() {
	// The certificate's primary key is certify-only; a signing subkey produced the signature. It must
	// verify (the subkey is signing-capable and validly bound) and be attributed to the *primary*
	// certificate fingerprint — the enrolled identity — not the subkey.
	let cert = TrustedKey::from_armored_pgp(SUBKEY_SIGNER_PUB).expect("parse subkey cert");
	let key = verify_commit::<Sha1>(SUBKEY_SIGNED_COMMIT, &[cert]).expect("verify");
	assert_eq!(key.as_str(), SUBKEY_SIGNER_FINGERPRINT);
}

#[test]
fn rejects_a_signature_whose_issuer_is_untrusted() {
	// Only an unrelated certificate is trusted: the issuer names no trusted key.
	match verify_commit::<Sha1>(SIGNED_COMMIT, &[other()]) {
		Err(TrustError::UntrustedKey(id)) => assert_eq!(id.as_str(), SIGNER_FINGERPRINT),
		other => panic!("expected UntrustedKey, got {other:?}"),
	}
}

#[test]
fn an_ssh_only_trust_set_does_not_verify_a_pgp_signature() {
	// The PGP path skips SSH entries entirely; with no PGP cert trusted, the signature is untrusted.
	let keys = [TrustedKey::from_openssh(SSH_PUB).expect("ssh key")];
	assert!(matches!(
		verify_commit::<Sha1>(SIGNED_COMMIT, &keys),
		Err(TrustError::UntrustedKey(_))
	));
}

#[test]
fn rejects_a_tampered_payload() {
	// Re-encode the commit with a changed message: the issuer still matches the trusted cert, but the
	// signed bytes differ, so the signature must fail as BadSignature (not UntrustedKey).
	let mut commit = parse_commit::<Sha1>(SIGNED_COMMIT).expect("parse");
	commit.message = "tampered\n".to_owned();
	let raw = encode_commit(&commit);
	assert!(matches!(
		verify_commit::<Sha1>(&raw, &[signer()]),
		Err(TrustError::BadSignature)
	));
}

#[test]
fn reports_an_unsigned_object() {
	let mut commit = parse_commit::<Sha1>(SIGNED_COMMIT).expect("parse");
	commit.signature = None;
	let raw = encode_commit(&commit);
	assert!(matches!(
		verify_commit::<Sha1>(&raw, &[signer()]),
		Err(TrustError::Unsigned)
	));
}

#[test]
fn rejects_a_malformed_pgp_signature_block() {
	let armor = b"-----BEGIN PGP SIGNATURE-----\n\nnot base64\n-----END PGP SIGNATURE-----\n";
	let err = verify_pgpsig(b"payload", armor, &[signer()]).expect_err("should reject");
	assert!(
		matches!(err, TrustError::MalformedPgpSignature(_)),
		"{err:?}"
	);
}

#[test]
fn trust_root_parses_a_document_mixing_ssh_and_pgp_keys() {
	// A trust document's key list may carry OpenSSH lines and armored OpenPGP certificates together;
	// folding into a TrustRoot must parse each by its armor. (The JSON string preserves the PGP
	// block's newlines.)
	let document = TrustDocument::new(
		1,
		Policy::Warn,
		vec![SSH_PUB.trim().to_owned(), SIGNER_PUB.to_owned()],
	);
	let root = TrustRoot::from_json(&document.to_json()).expect("parse mixed root");
	assert_eq!(root.keys.len(), 2);
	assert!(matches!(root.keys[0], TrustedKey::Ssh(_)));
	assert!(matches!(root.keys[1], TrustedKey::Pgp(_)));
	assert_eq!(root.keys[1].id().as_str(), SIGNER_FINGERPRINT);
}

#[test]
fn rejects_a_malformed_pgp_public_key() {
	let block =
		"-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nnot base64\n-----END PGP PUBLIC KEY BLOCK-----\n";
	let err = TrustedKey::from_armored_pgp(block).expect_err("should reject");
	assert!(matches!(err, TrustError::MalformedPgpKey(_)), "{err:?}");
}
