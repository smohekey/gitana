//! Trust and signature verification for gitana.
//!
//! This crate is the pure, I/O-free verification core of gitana's trust subsystem (see
//! `docs/hlds/secure-git-trust-signing.md`). It owns the rule that a signature over some bytes was
//! produced by a key the repository trusts — nothing about refs, storage, or the network.
//!
//! It verifies both **SSHSIG** signatures (`ssh-keygen -Y sign`, the format `git` writes for
//! `commit -S` / `tag -s` under `gpg.format=ssh`, in git's `git` namespace) and **OpenPGP**
//! signatures (git's default `gpg.format`). The object entry points dispatch on the signature's
//! armor to the matching path; a [`TrustedKey`] is correspondingly either an OpenSSH public key or
//! an OpenPGP certificate.
//!
//! The object entry points are [`verify_commit`] and [`verify_tag`], which take the **raw object
//! bytes** and verify the signature over exactly the bytes git signs. Verifying from the raw buffer
//! (rather than a re-encoded parsed object) is byte-accurate for commits carrying headers the object
//! model does not preserve — e.g. a merge of a signed tag, whose `mergetag` header git also signs.
//! [`verify_sshsig`] is the shared SSHSIG primitive the push-certificate path reuses; [`verify_pgpsig`]
//! is its OpenPGP counterpart. Each returns the [`KeyId`] that verified, so callers can audit *which*
//! trusted key signed.
//!
//! On top of that, [`fold_trust_root`] and [`verify_candidate_trust_update`] walk the
//! `refs/gitana/trust` commit chain (through an [`ObjectSource`]) into the effective [`TrustRoot`],
//! proving the whole authorization chain without touching any ref.

mod audit_event;
mod error;
mod fold;
mod folded_trust;
mod key_id;
mod object_source;
mod policy;
mod trust_document;
mod trust_root;
mod trusted_key;

pub use self::audit_event::AuditEvent;
pub use self::error::TrustError;
pub use self::fold::{
	fold_trust_root, fold_trust_root_anchored, verify_candidate_trust_update,
	verify_candidate_trust_update_anchored,
};
pub use self::folded_trust::FoldedTrust;
pub use self::key_id::KeyId;
pub use self::object_source::ObjectSource;
pub use self::policy::Policy;
pub use self::trust_document::TrustDocument;
pub use self::trust_root::{TRUST_DOCUMENT_PATH, TrustRoot};
pub use self::trusted_key::TrustedKey;

use gitana_object::{HashAlgorithm, commit_signature_and_payload, tag_signature_and_payload};
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey, SignedPublicSubKey};
use pgp::packet::{RevocationCode, Signature, SignatureType};
use pgp::types::{Duration, KeyDetails, Tag};
use ssh_key::{PublicKey, SshSig};

/// The SSHSIG namespace git uses for signed commits and tags (`ssh-keygen -Y sign -n git`).
const GIT_NAMESPACE: &str = "git";

/// The armor header of an OpenPGP detached signature block; anything else is treated as an SSHSIG.
const PGP_SIGNATURE_MARKER: &[u8] = b"-----BEGIN PGP SIGNATURE-----";

/// Verify a signed commit against `keys` from its raw object bytes: check its signature (in git's
/// `git` namespace, for SSHSIG) over the bytes git signs — the commit buffer with only its `gpgsig`
/// header removed, so headers like `mergetag` are covered. Works entirely on bytes, so a commit with
/// a non-UTF-8 message (an `encoding` header) still verifies. Dispatches on the signature's armor to
/// the SSHSIG or OpenPGP path. Returns the trusted [`KeyId`] that signed, or [`TrustError::Unsigned`]
/// if the commit carries no signature.
pub fn verify_commit<H: HashAlgorithm>(
	raw: &[u8],
	keys: &[TrustedKey],
) -> Result<KeyId, TrustError> {
	let (signature, payload) = commit_signature_and_payload::<H>(raw);
	let armor = signature.ok_or(TrustError::Unsigned)?;
	verify_object_signature(&payload, &armor, keys)
}

/// Verify a signed annotated tag against `keys` from its raw object bytes: check its signature (in
/// git's `git` namespace, for SSHSIG) over the bytes git signs — the tag without its appended
/// signature block. Works entirely on bytes, so a tag with a non-UTF-8 message still verifies.
/// Dispatches on the signature's armor to the SSHSIG or OpenPGP path. Returns the trusted [`KeyId`]
/// that signed, or [`TrustError::Unsigned`] if the tag carries no signature.
pub fn verify_tag(raw: &[u8], keys: &[TrustedKey]) -> Result<KeyId, TrustError> {
	let (signature, payload) = tag_signature_and_payload(raw);
	let armor = signature.ok_or(TrustError::Unsigned)?;
	verify_object_signature(&payload, &armor, keys)
}

/// Verify a *trust-chain* commit (a `refs/gitana/trust` update) against `keys`: like [`verify_commit`]
/// but **SSHSIG-only**. Trust-root updates are authorized exclusively by OpenSSH keys — an OpenPGP
/// certificate enrolled in a root is a verification-only anchor (it signs nothing in gitana), so a
/// PGP-signed trust commit is refused outright even when its cert is enrolled, and only
/// [`TrustedKey::Ssh`] entries can authorize the update. This keeps trust management SSH-only, matching
/// the porcelain (which signs trust updates with `ssh-keygen`) and the enrolment safety checks.
pub(crate) fn verify_trust_commit<H: HashAlgorithm>(
	raw: &[u8],
	keys: &[TrustedKey],
) -> Result<KeyId, TrustError> {
	let (signature, payload) = commit_signature_and_payload::<H>(raw);
	let armor = signature.ok_or(TrustError::Unsigned)?;
	if armor.starts_with(PGP_SIGNATURE_MARKER) {
		return Err(TrustError::BadSignature);
	}
	verify_sshsig(&payload, &armor, keys, GIT_NAMESPACE)
}

/// Verify an object signature (`armor`) over `payload` against `keys`, dispatching on the armor's
/// PEM header: an OpenPGP `-----BEGIN PGP SIGNATURE-----` block goes to [`verify_pgpsig`], anything
/// else is treated as an SSHSIG block in git's `git` namespace ([`verify_sshsig`]). A commit's or
/// tag's signature is always in the `git` namespace, so the namespace is fixed here.
fn verify_object_signature(
	payload: &[u8],
	armor: &[u8],
	keys: &[TrustedKey],
) -> Result<KeyId, TrustError> {
	if armor.starts_with(PGP_SIGNATURE_MARKER) {
		verify_pgpsig(payload, armor, keys)
	} else {
		verify_sshsig(payload, armor, keys, GIT_NAMESPACE)
	}
}

/// Verify a detached SSHSIG (`armor`, an `-----BEGIN SSH SIGNATURE-----` PEM block, as bytes) over
/// `payload` for `namespace` against `keys`.
///
/// The signature embeds its signer's public key; verification requires both that this key matches a
/// trusted entry *and* that the signature verifies over the payload. Returns the matching
/// [`KeyId`]; errors distinguish a malformed block, an untrusted signer (named by fingerprint), and
/// a signature that does not verify. Only [`TrustedKey::Ssh`] entries can match; PGP entries are
/// skipped.
pub fn verify_sshsig(
	payload: &[u8],
	armor: &[u8],
	keys: &[TrustedKey],
	namespace: &str,
) -> Result<KeyId, TrustError> {
	let signature = SshSig::from_pem(armor).map_err(TrustError::MalformedSignature)?;
	let signer = signature.public_key();
	match keys
		.iter()
		.find(|key| key.ssh().map(PublicKey::key_data) == Some(signer))
	{
		Some(key) => {
			// The caller has confirmed this key produced the signature; verify it over the payload.
			key
				.ssh()
				.expect("matched an ssh key above")
				.verify(namespace, payload, &signature)
				.map_err(|_| TrustError::BadSignature)?;
			Ok(key.id())
		}
		None => Err(TrustError::UntrustedKey(KeyId::of(signer))),
	}
}

/// Verify a detached OpenPGP signature (`armor`, an `-----BEGIN PGP SIGNATURE-----` PEM block, as
/// bytes) over `payload` against `keys`.
///
/// Unlike SSHSIG, an OpenPGP signature does not embed its signer's key — it names an *issuer* by
/// fingerprint (and key id). So this matches the signature's issuer against each trusted PGP
/// certificate's primary key and subkeys, then verifies the signature with the matched component
/// key. A matched-but-non-verifying signature is a [`TrustError::BadSignature`]; a signature whose
/// issuer names no trusted certificate is a [`TrustError::UntrustedKey`] (named by the issuer
/// fingerprint). The returned [`KeyId`] is the trusted certificate's fingerprint, even when a subkey
/// produced the signature — the certificate is the trusted identity.
///
/// This relies on the signature carrying an issuer subpacket, as every `git`/`gpg`-produced
/// signature does; a signature with no issuer identifier cannot be attributed and is rejected.
///
/// The matched component must be a *valid signer as of the signature's own creation time*: a
/// verified self-signature (primary) or subkey binding (subkey) grants it the signing key flag, that
/// grant has not expired at the signing time, and the component is not revoked. Validity is judged at
/// the signature's embedded timestamp — not a wall clock — so a commit validly signed while its key
/// was live stays valid, and no external clock has to be threaded through the trust core. Only the
/// *used* component is validated (`verify_bindings` over the whole certificate would reject an
/// otherwise-valid key that carries third-party User ID certifications).
pub fn verify_pgpsig(
	payload: &[u8],
	armor: &[u8],
	keys: &[TrustedKey],
) -> Result<KeyId, TrustError> {
	let (detached, _headers) = DetachedSignature::from_armor_single(std::io::Cursor::new(armor))
		.map_err(TrustError::MalformedPgpSignature)?;
	let signature = &detached.signature;
	// The signature must be a *binary data* signature — the type git produces over the object bytes.
	// Other types (a `Standalone` or `Timestamp` signature, a certification) do not hash the object
	// payload, so accepting one would let a signature a trusted key made over *other* bytes be pasted
	// into a `gpgsig` and pass for an arbitrary object. (Text mode is excluded too: git signs the exact
	// bytes, not line-ending-canonicalised text.)
	if signature.typ() != Some(SignatureType::Binary) {
		return Err(TrustError::BadSignature);
	}
	// The signing time anchors every validity window (expiry, binding freshness). A signature with no
	// creation timestamp cannot be placed in time, so it cannot be trusted.
	let signed_at = signature
		.created()
		.ok_or(TrustError::BadSignature)?
		.as_secs();
	let issuer_fingerprints = signature.issuer_fingerprint();
	let issuer_key_ids = signature.issuer_key_id();

	// Find the trusted component the signature claims as issuer, validate it as a signer at the signing
	// time, and verify the signature with it. The issuer *fingerprint* is unique, but the legacy issuer
	// *key id* is not — two trusted keys can share one — so a match that fails to validate or verify
	// must not fail fast: keep trying every issuer-matched component and accept the first that verifies,
	// failing only once all are exhausted.
	let mut matched_issuer = false;
	for key in keys {
		let Some(cert) = key.pgp() else { continue };
		let issued_by = |fingerprint: &pgp::types::Fingerprint, key_id: &pgp::types::KeyId| {
			issuer_fingerprints.iter().any(|f| *f == fingerprint)
				|| issuer_key_ids.iter().any(|k| *k == key_id)
		};
		if issued_by(&cert.fingerprint(), &cert.legacy_key_id()) {
			matched_issuer = true;
			if primary_valid_signer_at(cert, signed_at) && detached.verify(cert, payload).is_ok() {
				return Ok(key.id());
			}
		}
		for subkey in &cert.public_subkeys {
			if issued_by(&subkey.fingerprint(), &subkey.legacy_key_id()) {
				matched_issuer = true;
				if subkey_valid_signer_at(cert, subkey, signed_at)
					&& detached.verify(subkey, payload).is_ok()
				{
					return Ok(key.id());
				}
			}
		}
	}

	// Nothing verified. A trusted component matched the issuer but failed to validate/verify → the
	// signature is bad. Otherwise no trusted key claims it: name the issuer fingerprint if present.
	if matched_issuer {
		return Err(TrustError::BadSignature);
	}
	match issuer_fingerprints.first() {
		Some(fingerprint) => Err(TrustError::UntrustedKey(KeyId::of_pgp(fingerprint))),
		None => Err(TrustError::BadSignature),
	}
}

/// Whether the certificate's *primary* key was a valid signer at `signed_at` (unix seconds): it is
/// not revoked, and its *effective* self-signature at `signed_at` grants the signing key flag and has
/// not expired. The effective self-signature is the newest verified one (a User ID certification or a
/// direct-key signature) that already existed at `signed_at` — a later self-signature supersedes an
/// earlier grant, so a downgrade (dropping the sign flag or shortening the expiry) is honoured rather
/// than bypassed via a stale grant. Only self-signatures that verify against the primary are
/// considered, so third-party certifications neither authorize nor block it.
fn primary_valid_signer_at(cert: &SignedPublicKey, signed_at: u32) -> bool {
	let primary = &cert.primary_key;
	// A verified primary-key revocation that applies at `signed_at` makes it unusable.
	if cert
		.details
		.revocation_signatures
		.iter()
		.any(|sig| sig.verify_key(primary).is_ok() && revocation_applies_at(sig, signed_at))
	{
		return false;
	}
	let created_at = primary.created_at().as_secs();
	// Verified self-signatures (User ID certifications and direct-key signatures) that existed at
	// `signed_at`; the newest is effective.
	let via_user = cert.details.users.iter().flat_map(|user| {
		user.signatures.iter().filter(move |sig| {
			sig
				.verify_certification(primary, Tag::UserId, &user.id)
				.is_ok()
		})
	});
	let via_direct = cert
		.details
		.direct_signatures
		.iter()
		.filter(|sig| sig.verify_key(primary).is_ok());
	match effective_grant(via_user.chain(via_direct), signed_at) {
		Some(grant) => {
			grant.key_flags().sign() && !expired_at(created_at, grant.key_expiration_time(), signed_at)
		}
		None => false,
	}
}

/// The newest signing grant (self-signature or subkey binding) among `grants` that already existed at
/// `signed_at` — the *effective* one, whose flags and expiry govern. A later grant supersedes an
/// earlier one.
fn effective_grant<'a>(
	grants: impl Iterator<Item = &'a Signature>,
	signed_at: u32,
) -> Option<&'a Signature> {
	grants
		.filter(|sig| existed_at(sig, signed_at))
		.max_by_key(|sig| sig.created().map_or(0, |created| created.as_secs()))
}

/// Whether `subkey` was a valid signer of `cert` at `signed_at` (unix seconds): it is not revoked,
/// and its *effective* binding to the primary at `signed_at` is signing-capable (with the back-
/// signature a signing subkey must carry) and unexpired. The effective binding is the newest verified
/// subkey-binding that already existed at `signed_at`, so a later rebind that drops the sign flag or
/// shortens the expiry is honoured rather than bypassed via a stale earlier binding. Validating only
/// this subkey's own bindings (not the whole certificate) leaves an otherwise-valid key carrying
/// third-party certifications usable.
fn subkey_valid_signer_at(
	cert: &SignedPublicKey,
	subkey: &SignedPublicSubKey,
	signed_at: u32,
) -> bool {
	let primary = &cert.primary_key;
	// A verified subkey revocation that applies at `signed_at` makes it unusable.
	if subkey.signatures.iter().any(|sig| {
		sig.typ() == Some(SignatureType::SubkeyRevocation)
			&& sig.verify_subkey_binding(primary, &subkey.key).is_ok()
			&& revocation_applies_at(sig, signed_at)
	}) {
		return false;
	}
	let created_at = subkey.key.created_at().as_secs();
	let bindings = subkey.signatures.iter().filter(|sig| {
		sig.typ() == Some(SignatureType::SubkeyBinding)
			&& sig.verify_subkey_binding(primary, &subkey.key).is_ok()
	});
	match effective_grant(bindings, signed_at) {
		Some(binding) => {
			binding.key_flags().sign()
				// A signing subkey binding must embed a back-signature (a primary-key binding made by the
				// subkey), proving the subkey consents to being bound — without it the binding is forgeable.
				&& binding
					.embedded_signature()
					.is_some_and(|back| back.verify_primary_key_binding(&subkey.key, primary).is_ok())
				&& !expired_at(created_at, binding.key_expiration_time(), signed_at)
		}
		None => false,
	}
}

/// Whether `revocation` invalidates a signature created at `signed_at` (unix seconds), consistent
/// with the timestamp-anchored validity model. A key that was merely *superseded* or *retired* was
/// not compromised, so its revocation only invalidates signatures made at or after the revocation —
/// earlier signatures that were valid when made stay valid. A *compromise* (or an unspecified reason)
/// is retroactive: the key may have signed maliciously before it was revoked, so it invalidates every
/// signature regardless of time.
fn revocation_applies_at(revocation: &Signature, signed_at: u32) -> bool {
	match revocation.revocation_reason_code().copied() {
		Some(RevocationCode::KeySuperseded | RevocationCode::KeyRetired) => revocation
			.created()
			.is_none_or(|created| created.as_secs() <= signed_at),
		_ => true,
	}
}

/// Whether a signing grant `sig` (a self-signature or subkey binding) already existed at `signed_at`
/// (unix seconds) — its creation time is present and at or before the object signature's. A grant
/// made *after* the object was signed cannot retroactively authorize it; a grant with no creation
/// time cannot be placed in time and is not honoured.
fn existed_at(sig: &Signature, signed_at: u32) -> bool {
	sig
		.created()
		.is_some_and(|created| created.as_secs() <= signed_at)
}

/// Whether a component created at `created_at` (unix seconds) with the key-expiration `duration` from
/// its self-signature/binding had expired by `signed_at`. A missing or zero duration means no
/// expiration.
fn expired_at(created_at: u32, duration: Option<Duration>, signed_at: u32) -> bool {
	match duration {
		Some(duration) if duration.as_secs() != 0 => {
			let expires_at = u64::from(created_at) + u64::from(duration.as_secs());
			u64::from(signed_at) > expires_at
		}
		_ => false,
	}
}

#[cfg(test)]
mod tests {
	use gitana_object::Sha1;
	use pgp::types::Duration;

	use super::{TrustError, TrustedKey, expired_at, verify_trust_commit};

	#[test]
	fn trust_commit_verification_refuses_a_pgp_signature() {
		// A trust-chain commit is SSHSIG-only: even a *valid* OpenPGP signature by an *enrolled* cert
		// must not authorize a trust update (where `verify_commit` would accept it). Fixtures are the
		// OpenPGP verify fixtures.
		let cert =
			TrustedKey::from_armored_pgp(include_str!("../tests/fixtures/pgp_signer.pub.asc")).unwrap();
		let pgp_commit = include_bytes!("../tests/fixtures/signed_commit_pgp.obj");
		assert!(matches!(
			verify_trust_commit::<Sha1>(pgp_commit, &[cert]),
			Err(TrustError::BadSignature)
		));
	}

	#[test]
	fn expiry_is_relative_to_creation_and_signing_time() {
		let created = 1_000;
		// No expiration (None) or an explicit zero duration never expires.
		assert!(!expired_at(created, None, 9_999_999));
		assert!(!expired_at(
			created,
			Some(Duration::from_secs(0)),
			9_999_999
		));
		// Expires at created + duration = 1_100. A signature at or before that is still valid.
		assert!(!expired_at(created, Some(Duration::from_secs(100)), 1_050));
		assert!(!expired_at(created, Some(Duration::from_secs(100)), 1_100));
		// A signature strictly after the expiry is expired.
		assert!(expired_at(created, Some(Duration::from_secs(100)), 1_101));
	}
}
