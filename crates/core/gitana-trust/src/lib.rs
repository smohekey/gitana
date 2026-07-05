//! Trust and signature verification for gitana.
//!
//! This crate is the pure, I/O-free verification core of gitana's trust subsystem (see
//! `docs/hlds/secure-git-trust-signing.md`). It owns the rule that a signature over some bytes was
//! produced by a key the repository trusts — nothing about refs, storage, or the network.
//!
//! v1 verifies **SSHSIG** signatures only (`ssh-keygen -Y sign`, the format `git` writes for
//! `commit -S` / `tag -s` under `gpg.format=ssh`), with git's `git` namespace. OpenPGP is a later,
//! additive concern.
//!
//! The entry points are [`verify_commit`] and [`verify_tag`], which take the **raw object bytes**
//! and verify the signature over exactly the bytes git signs. Verifying from the raw buffer (rather
//! than a re-encoded parsed object) is byte-accurate for commits carrying headers the object model
//! does not preserve — e.g. a merge of a signed tag, whose `mergetag` header git also signs.
//! [`verify_sshsig`] is the shared primitive the push-certificate path will reuse. Each returns the
//! [`KeyId`] that verified, so callers can audit *which* trusted key signed.

mod error;
mod key_id;
mod trusted_key;

pub use self::error::TrustError;
pub use self::key_id::KeyId;
pub use self::trusted_key::TrustedKey;

use gitana_object::{HashAlgorithm, commit_signature_and_payload, tag_signature_and_payload};
use ssh_key::SshSig;

/// The SSHSIG namespace git uses for signed commits and tags (`ssh-keygen -Y sign -n git`).
const GIT_NAMESPACE: &str = "git";

/// Verify a signed commit against `keys` from its raw object bytes: check its SSHSIG (git's `git`
/// namespace) over the bytes git signs — the commit buffer with only its `gpgsig` header removed,
/// so headers like `mergetag` are covered. Works entirely on bytes, so a commit with a non-UTF-8
/// message (an `encoding` header) still verifies. Returns the trusted [`KeyId`] that signed, or
/// [`TrustError::Unsigned`] if the commit carries no signature.
pub fn verify_commit<H: HashAlgorithm>(
	raw: &[u8],
	keys: &[TrustedKey],
) -> Result<KeyId, TrustError> {
	let (signature, payload) = commit_signature_and_payload::<H>(raw);
	let armor = signature.ok_or(TrustError::Unsigned)?;
	verify_sshsig(&payload, &armor, keys, GIT_NAMESPACE)
}

/// Verify a signed annotated tag against `keys` from its raw object bytes: check its SSHSIG (git's
/// `git` namespace) over the bytes git signs — the tag without its appended signature block. Works
/// entirely on bytes, so a tag with a non-UTF-8 message still verifies. Returns the trusted
/// [`KeyId`] that signed, or [`TrustError::Unsigned`] if the tag carries no signature.
pub fn verify_tag(raw: &[u8], keys: &[TrustedKey]) -> Result<KeyId, TrustError> {
	let (signature, payload) = tag_signature_and_payload(raw);
	let armor = signature.ok_or(TrustError::Unsigned)?;
	verify_sshsig(&payload, &armor, keys, GIT_NAMESPACE)
}

/// Verify a detached SSHSIG (`armor`, an `-----BEGIN SSH SIGNATURE-----` PEM block, as bytes) over
/// `payload` for `namespace` against `keys`.
///
/// The signature embeds its signer's public key; verification requires both that this key matches a
/// trusted entry *and* that the signature verifies over the payload. Returns the matching
/// [`KeyId`]; errors distinguish a malformed block, an untrusted signer (named by fingerprint), and
/// a signature that does not verify.
pub fn verify_sshsig(
	payload: &[u8],
	armor: &[u8],
	keys: &[TrustedKey],
	namespace: &str,
) -> Result<KeyId, TrustError> {
	let signature = SshSig::from_pem(armor).map_err(TrustError::MalformedSignature)?;
	let signer = signature.public_key();
	match keys.iter().find(|key| key.key_data() == signer) {
		Some(key) => {
			key.verify(namespace, payload, &signature)?;
			Ok(key.id())
		}
		None => Err(TrustError::UntrustedKey(KeyId::of(signer))),
	}
}
