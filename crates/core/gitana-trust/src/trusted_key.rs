use pgp::composed::{Deserializable, SignedPublicKey};
use pgp::types::KeyDetails;
use ssh_key::PublicKey;

use crate::{KeyId, TrustError};

/// The armor header a trust-document entry uses to carry an OpenPGP public-key certificate, as
/// opposed to a single-line OpenSSH public key.
const PGP_PUBLIC_KEY_MARKER: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";

/// A public key the repository trusts to sign objects and push certificates. Either an OpenSSH
/// public key (SSHSIG signatures) or an OpenPGP public-key certificate (OpenPGP signatures); its
/// [`KeyId`] (fingerprint) is used for matching and audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedKey {
	/// An OpenSSH public key — verifies SSHSIG signatures (`ssh-keygen -Y sign`, git's
	/// `gpg.format=ssh`).
	Ssh(PublicKey),
	/// An OpenPGP public-key certificate — verifies OpenPGP signatures (git's default `gpg.format`).
	/// Boxed because a [`SignedPublicKey`] is large relative to a [`PublicKey`].
	Pgp(Box<SignedPublicKey>),
}

impl TrustedKey {
	/// Parse a trusted key from a trust-document entry, dispatching on its armor: an OpenPGP
	/// `-----BEGIN PGP PUBLIC KEY BLOCK-----` certificate, otherwise a single-line OpenSSH public key
	/// (`ssh-ed25519 AAAA… comment`, as in `authorized_keys`).
	pub fn parse(entry: &str) -> Result<Self, TrustError> {
		if entry.trim_start().starts_with(PGP_PUBLIC_KEY_MARKER) {
			Self::from_armored_pgp(entry)
		} else {
			Self::from_openssh(entry.trim())
		}
	}

	/// Parse a trusted key from an OpenSSH public-key line (`ssh-ed25519 AAAA… comment`), as it
	/// appears in `authorized_keys` and a trust document's key list.
	pub fn from_openssh(line: &str) -> Result<Self, TrustError> {
		PublicKey::from_openssh(line)
			.map(Self::Ssh)
			.map_err(TrustError::MalformedKey)
	}

	/// Parse a trusted key from an armored OpenPGP public-key certificate
	/// (`-----BEGIN PGP PUBLIC KEY BLOCK-----`), as `gpg --export --armor` prints and a trust
	/// document's key list may carry.
	pub fn from_armored_pgp(block: &str) -> Result<Self, TrustError> {
		let (key, _headers) =
			SignedPublicKey::from_string(block).map_err(TrustError::MalformedPgpKey)?;
		// Note: certificate validity is *not* fully checked here. `verify_bindings` over the whole
		// certificate would reject an otherwise-valid key that carries third-party User ID
		// certifications (common in `gpg --export` output), and it would not capture revocation or
		// expiry. Instead the *used* component (primary or the specific signing subkey) is validated at
		// verify time — binding, back-signature, signing flag, revocation, and expiry as of the
		// signature's creation time — see `verify_pgpsig`. So enrolment accepts any parseable
		// certificate; a certificate that can never validly sign simply never verifies anything.
		Ok(Self::Pgp(Box::new(key)))
	}

	/// This key's identity: an OpenSSH SHA-256 fingerprint (`SHA256:…`) for an SSH key, or the
	/// certificate's OpenPGP fingerprint (uppercase hex) for a PGP key.
	pub fn id(&self) -> KeyId {
		match self {
			Self::Ssh(key) => KeyId::of(key.key_data()),
			Self::Pgp(cert) => KeyId::of_pgp(&cert.fingerprint()),
		}
	}

	/// The underlying OpenSSH key, if this is an SSH key — the SSHSIG path matches the signer's
	/// embedded key against it.
	pub(crate) fn ssh(&self) -> Option<&PublicKey> {
		match self {
			Self::Ssh(key) => Some(key),
			Self::Pgp(_) => None,
		}
	}

	/// The underlying OpenPGP certificate, if this is a PGP key — the OpenPGP path matches the
	/// signature's issuer against it and its subkeys.
	pub(crate) fn pgp(&self) -> Option<&SignedPublicKey> {
		match self {
			Self::Pgp(cert) => Some(cert),
			Self::Ssh(_) => None,
		}
	}
}
