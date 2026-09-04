use gitana_object::HashKind;

/// A runtime-tagged object id returned by hash-generic submodule operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleObjectId {
	kind: HashKind,
	hex: String,
}

impl SubmoduleObjectId {
	#[cfg(not(target_arch = "wasm32"))]
	/// Construct a runtime-tagged object id from a typed Git object id.
	pub fn from_typed<H: gitana_object::HashAlgorithm>(oid: gitana_object::ObjectId<H>) -> Self {
		Self {
			kind: kind::<H>(),
			hex: oid.to_hex(),
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn to_typed<H: gitana_object::HashAlgorithm>(
		&self,
	) -> Option<gitana_object::ObjectId<H>> {
		if self.kind != kind::<H>() {
			return None;
		}
		gitana_object::ObjectId::from_hex(&self.hex).ok()
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn zero<H: gitana_object::HashAlgorithm>() -> Self {
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

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn kind<H: gitana_object::HashAlgorithm>() -> HashKind {
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
