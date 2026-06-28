use std::fmt;

use sha2::{Digest, Sha256};

use crate::{ObjectError, ObjectKind};

/// A git object id: the SHA-256 of the canonical `<kind> <size>\0<payload>` form.
///
/// 32 raw bytes; rendered as 64 lowercase hex characters. There is no hash-kind
/// type parameter — gitana is SHA-256 only (see docs/hlds/storage-layer.md).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
	/// Compute the id of an object from its kind and payload.
	pub fn compute(kind: ObjectKind, payload: &[u8]) -> Self {
		let mut hasher = Sha256::new();
		hasher.update(kind.as_str().as_bytes());
		hasher.update(b" ");
		hasher.update(payload.len().to_string().as_bytes());
		hasher.update(b"\0");
		hasher.update(payload);
		ObjectId(hasher.finalize().into())
	}

	/// Wrap 32 raw digest bytes (e.g. read from a tree entry).
	pub fn from_bytes(bytes: [u8; 32]) -> Self {
		ObjectId(bytes)
	}

	/// The raw 32-byte digest.
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// The lowercase hex rendering (64 characters).
	pub fn to_hex(self) -> String {
		let mut s = String::with_capacity(64);
		for byte in self.0 {
			s.push_str(&format!("{byte:02x}"));
		}
		s
	}

	/// Parse a 64-character lowercase hex string.
	pub fn from_hex(hex: &str) -> Result<Self, ObjectError> {
		if hex.len() != 64 {
			return Err(ObjectError::InvalidObjectId);
		}
		let mut bytes = [0u8; 32];
		for (i, byte) in bytes.iter_mut().enumerate() {
			let pair = &hex[i * 2..i * 2 + 2];
			*byte = u8::from_str_radix(pair, 16).map_err(|_| ObjectError::InvalidObjectId)?;
		}
		Ok(ObjectId(bytes))
	}
}

impl fmt::Display for ObjectId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_hex())
	}
}

impl fmt::Debug for ObjectId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "ObjectId({})", self.to_hex())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hex_round_trips() {
		let id = ObjectId::compute(ObjectKind::Blob, b"hello");
		let back = ObjectId::from_hex(&id.to_hex()).expect("valid hex");
		assert_eq!(id, back);
		assert_eq!(id.to_hex().len(), 64);
	}

	#[test]
	fn empty_blob_matches_git_sha256() {
		// git's SHA-256 empty blob id (`blob 0\0`), a fixed, externally checkable value.
		let id = ObjectId::compute(ObjectKind::Blob, b"");
		assert_eq!(
			id.to_hex(),
			"473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
		);
	}
}
