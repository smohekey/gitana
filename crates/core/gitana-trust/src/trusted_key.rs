use ssh_key::PublicKey;
use ssh_key::public::KeyData;

use crate::{KeyId, TrustError};

/// A public key the repository trusts to sign objects and push certificates. Wraps an SSH public
/// key and exposes its [`KeyId`] (fingerprint) for matching and audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
	key: PublicKey,
}

impl TrustedKey {
	/// Parse a trusted key from an OpenSSH public-key line (`ssh-ed25519 AAAA… comment`), as it
	/// appears in `authorized_keys` and a trust document's key list.
	pub fn from_openssh(line: &str) -> Result<Self, TrustError> {
		PublicKey::from_openssh(line)
			.map(Self::new)
			.map_err(TrustError::MalformedKey)
	}

	/// Wrap an already-parsed SSH public key.
	pub fn new(key: PublicKey) -> Self {
		Self { key }
	}

	/// This key's identity: its SHA-256 fingerprint (`SHA256:…`).
	pub fn id(&self) -> KeyId {
		KeyId::of(self.key.key_data())
	}

	/// The underlying key material, used to match against a signature's embedded signer key.
	pub(crate) fn key_data(&self) -> &KeyData {
		self.key.key_data()
	}

	/// Verify a parsed SSHSIG over `payload` for `namespace` against this key. The caller has
	/// already confirmed this key produced the signature.
	pub(crate) fn verify(
		&self,
		namespace: &str,
		payload: &[u8],
		signature: &ssh_key::SshSig,
	) -> Result<(), TrustError> {
		self
			.key
			.verify(namespace, payload, signature)
			.map_err(|_| TrustError::BadSignature)
	}
}
