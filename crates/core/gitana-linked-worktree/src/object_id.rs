//! The runtime-tagged object id that crosses the library boundary.
//!
//! The engine is compile-time generic over the hash algorithm (`ObjectId<H>`), but Code Henge manages
//! repositories of mixed formats and must stay format-agnostic. So object ids leave the crate as a
//! [`WorktreeObjectId`] — an enum carrying the algorithm as a runtime tag — never as `ObjectId<H>`.

use gitana_object::{HashKind, ObjectId, Sha1, Sha256};

/// A resolved git object id whose hash algorithm is a runtime fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeObjectId {
	/// A SHA-1 object id.
	Sha1(ObjectId<Sha1>),
	/// A SHA-256 object id.
	Sha256(ObjectId<Sha256>),
}

impl WorktreeObjectId {
	/// The algorithm this id is expressed in.
	pub fn kind(&self) -> HashKind {
		match self {
			WorktreeObjectId::Sha1(_) => HashKind::Sha1,
			WorktreeObjectId::Sha256(_) => HashKind::Sha256,
		}
	}

	/// The lowercase hex form (40 chars for SHA-1, 64 for SHA-256).
	pub fn to_hex(&self) -> String {
		match self {
			WorktreeObjectId::Sha1(id) => (*id).to_hex(),
			WorktreeObjectId::Sha256(id) => (*id).to_hex(),
		}
	}

	/// Parse a hex object id for a given algorithm — how a caller supplies a known start commit to
	/// [`classify`](crate::classify).
	pub fn parse(kind: HashKind, hex: &str) -> Result<Self, crate::LinkedWorktreeError> {
		let invalid = || crate::LinkedWorktreeError::InvalidObjectId {
			kind,
			hex: hex.to_owned(),
		};
		match kind {
			HashKind::Sha1 => ObjectId::<Sha1>::from_hex(hex)
				.map(WorktreeObjectId::Sha1)
				.map_err(|_| invalid()),
			HashKind::Sha256 => ObjectId::<Sha256>::from_hex(hex)
				.map(WorktreeObjectId::Sha256)
				.map_err(|_| invalid()),
		}
	}
}

/// Tag a compile-time `ObjectId<H>` with its runtime algorithm. Implemented for the two concrete id
/// types so a monomorphized `..._generic::<H>` body can wrap the ids it resolves without matching on
/// `H`; a generic fn bounds `where ObjectId<H>: IntoWorktreeObjectId` and calls `.tag()`. Only the
/// native reading layer tags ids, so it is native-only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) trait IntoWorktreeObjectId {
	fn tag(self) -> WorktreeObjectId;
}

#[cfg(not(target_arch = "wasm32"))]
impl IntoWorktreeObjectId for ObjectId<Sha1> {
	fn tag(self) -> WorktreeObjectId {
		WorktreeObjectId::Sha1(self)
	}
}

#[cfg(not(target_arch = "wasm32"))]
impl IntoWorktreeObjectId for ObjectId<Sha256> {
	fn tag(self) -> WorktreeObjectId {
		WorktreeObjectId::Sha256(self)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_round_trips_both_algorithms() {
		let sha1 = "0123456789abcdef0123456789abcdef01234567";
		let id = WorktreeObjectId::parse(HashKind::Sha1, sha1).unwrap();
		assert_eq!(id.kind(), HashKind::Sha1);
		assert_eq!(id.to_hex(), sha1);

		let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
		let id = WorktreeObjectId::parse(HashKind::Sha256, sha256).unwrap();
		assert_eq!(id.kind(), HashKind::Sha256);
		assert_eq!(id.to_hex(), sha256);
	}

	#[test]
	fn parse_rejects_wrong_width() {
		// A SHA-1-length hex is not a valid SHA-256 id.
		let sha1 = "0123456789abcdef0123456789abcdef01234567";
		assert!(WorktreeObjectId::parse(HashKind::Sha256, sha1).is_err());
	}
}
