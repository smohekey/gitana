use std::fmt;

use ssh_key::{HashAlg, public::KeyData};

/// The stable identity of a trusted key: its OpenSSH SHA-256 fingerprint (`SHA256:…`), the same
/// string `ssh-keygen -lf` and `git`'s signature output print. Used to name the key that produced
/// (or failed to produce) a trusted signature, for audit and error reporting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyId(String);

impl KeyId {
	/// The SHA-256 fingerprint of an SSH public key.
	pub(crate) fn of(key: &KeyData) -> Self {
		Self(key.fingerprint(HashAlg::Sha256).to_string())
	}

	/// The fingerprint string (`SHA256:…`).
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for KeyId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}
