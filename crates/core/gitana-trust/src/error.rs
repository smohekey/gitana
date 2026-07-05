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
	/// Reading a trust object through the [`crate::ObjectSource`] failed.
	#[error("reading object {id}: {source}")]
	ObjectSource {
		/// The object id that could not be read.
		id: String,
		/// The backend read error.
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
	/// A trust document could not be parsed as JSON.
	#[error("malformed trust document: {0}")]
	MalformedTrustDocument(#[source] serde_json::Error),
	/// A trust root enrols no keys (an empty-key root is never accepted).
	#[error("trust root has no keys")]
	EmptyTrustRoot,
	/// The `refs/gitana/trust` chain is structurally invalid: a non-commit/-tree/-blob object, a
	/// missing trust document, a non-linear (merge) chain, or a divergent candidate update.
	#[error("invalid trust chain: {0}")]
	TrustChain(String),
}
