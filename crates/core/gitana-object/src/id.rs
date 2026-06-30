use std::fmt;

use crate::{HashAlgorithm, ObjectError, ObjectKind};

/// A git object id: the digest of the canonical `<kind> <size>\0<payload>` form under
/// the hash algorithm `H`.
///
/// Generic over `H` (e.g. [`crate::Sha1`], [`crate::Sha256`]): the algorithm fixes both
/// the raw width ([`HashAlgorithm::RAW_LEN`], 20 or 32 bytes) and the hex rendering
/// length (40 or 64). Storage is exactly [`H::Output`](HashAlgorithm::Output), so an id
/// is no wider than its hash, and equality, ordering, and hashing only ever compare ids
/// of the same `H`.
pub struct ObjectId<H: HashAlgorithm>(H::Output);

impl<H: HashAlgorithm> ObjectId<H> {
	/// Compute the id of an object from its kind and payload.
	pub fn compute(kind: ObjectKind, payload: &[u8]) -> Self {
		let len = payload.len().to_string();
		let raw = H::digest(&[
			kind.as_str().as_bytes(),
			b" ",
			len.as_bytes(),
			b"\0",
			payload,
		]);
		ObjectId(raw)
	}

	/// Wrap raw digest bytes (e.g. read from a tree entry or pack). The slice length must
	/// equal [`HashAlgorithm::RAW_LEN`] for `H`.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self, ObjectError> {
		if bytes.len() != H::RAW_LEN {
			return Err(ObjectError::InvalidObjectId);
		}
		let mut raw = H::Output::default();
		raw.as_mut().copy_from_slice(bytes);
		Ok(ObjectId(raw))
	}

	/// The raw digest bytes ([`HashAlgorithm::RAW_LEN`] of them).
	pub fn as_bytes(&self) -> &[u8] {
		self.0.as_ref()
	}

	/// The lowercase hex rendering (`2 * RAW_LEN` characters).
	pub fn to_hex(self) -> String {
		let mut s = String::with_capacity(H::RAW_LEN * 2);
		for byte in self.0.as_ref() {
			s.push_str(&format!("{byte:02x}"));
		}
		s
	}

	/// Parse a lowercase hex string of exactly `2 * RAW_LEN` characters.
	pub fn from_hex(hex: &str) -> Result<Self, ObjectError> {
		if hex.len() != H::RAW_LEN * 2 {
			return Err(ObjectError::InvalidObjectId);
		}
		let mut raw = H::Output::default();
		for (i, byte) in raw.as_mut().iter_mut().enumerate() {
			let pair = &hex[i * 2..i * 2 + 2];
			*byte = u8::from_str_radix(pair, 16).map_err(|_| ObjectError::InvalidObjectId)?;
		}
		Ok(ObjectId(raw))
	}
}

// These impls are written by hand rather than derived: a `#[derive]` would add a `H:
// Clone`/`H: Eq`/… bound that does not by itself prove `H::Output: Clone`/…; the trait
// already guarantees those bounds on `Output`, so the hand-written impls just need `H:
// HashAlgorithm`.
impl<H: HashAlgorithm> Clone for ObjectId<H> {
	fn clone(&self) -> Self {
		*self
	}
}

impl<H: HashAlgorithm> Copy for ObjectId<H> {}

impl<H: HashAlgorithm> PartialEq for ObjectId<H> {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}

impl<H: HashAlgorithm> Eq for ObjectId<H> {}

impl<H: HashAlgorithm> std::hash::Hash for ObjectId<H> {
	fn hash<S: std::hash::Hasher>(&self, state: &mut S) {
		self.0.hash(state);
	}
}

impl<H: HashAlgorithm> PartialOrd for ObjectId<H> {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl<H: HashAlgorithm> Ord for ObjectId<H> {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.0.cmp(&other.0)
	}
}

impl<H: HashAlgorithm> fmt::Display for ObjectId<H> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_hex())
	}
}

impl<H: HashAlgorithm> fmt::Debug for ObjectId<H> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "ObjectId({})", self.to_hex())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Sha1, Sha256};

	#[test]
	fn hex_round_trips() {
		let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"hello");
		let back = ObjectId::<Sha256>::from_hex(&id.to_hex()).expect("valid hex");
		assert_eq!(id, back);
		assert_eq!(id.to_hex().len(), 64);
	}

	#[test]
	fn empty_blob_matches_git_sha256() {
		// git's SHA-256 empty blob id (`blob 0\0`), a fixed, externally checkable value.
		let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"");
		assert_eq!(
			id.to_hex(),
			"473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
		);
	}

	#[test]
	fn empty_blob_matches_git_sha1() {
		// git's classic SHA-1 empty blob id, a fixed, externally checkable value.
		let id = ObjectId::<Sha1>::compute(ObjectKind::Blob, b"");
		assert_eq!(id.to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
		assert_eq!(id.to_hex().len(), 40);
	}

	#[test]
	fn sha1_hex_round_trips() {
		let id = ObjectId::<Sha1>::compute(ObjectKind::Blob, b"hello");
		let back = ObjectId::<Sha1>::from_hex(&id.to_hex()).expect("valid hex");
		assert_eq!(id, back);
	}

	#[test]
	fn raw_width_is_exact() {
		// An id stores exactly its hash width — no padding.
		assert_eq!(
			ObjectId::<Sha1>::compute(ObjectKind::Blob, b"x")
				.as_bytes()
				.len(),
			20
		);
		assert_eq!(
			ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x")
				.as_bytes()
				.len(),
			32
		);
	}

	#[test]
	fn from_hex_rejects_wrong_length() {
		// A 40-char string is not a valid SHA-256 id, and vice versa.
		assert!(ObjectId::<Sha256>::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").is_err());
		assert!(
			ObjectId::<Sha1>::from_hex(
				"473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
			)
			.is_err()
		);
	}
}
