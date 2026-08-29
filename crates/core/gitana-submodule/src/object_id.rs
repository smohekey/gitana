use gitana_object::{HashAlgorithm, HashKind, ObjectId};

/// A runtime-tagged object id returned by hash-generic submodule operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleObjectId {
	kind: HashKind,
	hex: String,
}

impl SubmoduleObjectId {
	pub(crate) fn from_typed<H: HashAlgorithm>(oid: ObjectId<H>) -> Self {
		Self {
			kind: kind::<H>(),
			hex: oid.to_hex(),
		}
	}

	pub(crate) fn zero<H: HashAlgorithm>() -> Self {
		Self {
			kind: kind::<H>(),
			hex: "0".repeat(H::RAW_LEN * 2),
		}
	}

	pub fn kind(&self) -> HashKind {
		self.kind
	}

	pub fn as_hex(&self) -> &str {
		&self.hex
	}
}

pub(crate) fn kind<H: HashAlgorithm>() -> HashKind {
	match H::NAME {
		"sha1" => HashKind::Sha1,
		"sha256" => HashKind::Sha256,
		_ => unreachable!("HashAlgorithm is sealed to Git-supported algorithms"),
	}
}

impl std::fmt::Display for SubmoduleObjectId {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(&self.hex)
	}
}
