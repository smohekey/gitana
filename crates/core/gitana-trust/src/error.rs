use crate::KeyId;

/// Why verifying an object (or bare) signature against a set of trusted keys failed.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
	/// The object carries no signature to verify.
	#[error("object is not signed")]
	Unsigned,
	/// The armored signature block could not be parsed as an SSHSIG.
	#[error("malformed signature: {0}")]
	MalformedSignature(#[source] ssh_key::Error),
	/// A trusted-key entry could not be parsed as an OpenSSH public key.
	#[error("malformed public key: {0}")]
	MalformedKey(#[source] ssh_key::Error),
	/// The signature is cryptographically valid but its signer is not in the trusted set.
	#[error("signature by untrusted key {0}")]
	UntrustedKey(KeyId),
	/// The signer is trusted but the signature does not verify over the payload (bad signature,
	/// tampered payload, or wrong namespace).
	#[error("signature does not verify")]
	BadSignature,
}
