use crate::{Policy, TrustDocument, TrustError, TrustedKey};

/// The path, within a trust commit's tree, of the canonical trust document.
pub const TRUST_DOCUMENT_PATH: &str = "trust.json";

/// A folded trust root: the effective trust state (enrolled keys and policy) at some point in the
/// `refs/gitana/trust` chain. Built from the JSON trust document a trust commit's tree carries.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustRoot {
	/// The document schema version.
	pub version: u32,
	/// The enforcement policy (see [`Policy`]).
	pub policy: Policy,
	/// The trusted signing keys. Never empty — an empty-key root is refused.
	pub keys: Vec<TrustedKey>,
	/// Free-form document metadata, preserved but not interpreted in v1.
	pub metadata: serde_json::Value,
}

impl TrustRoot {
	/// Parse a trust root from its canonical JSON document bytes. Errors on malformed JSON, an
	/// unparseable key, or an empty key set (an empty-key root is never accepted).
	pub fn from_json(bytes: &[u8]) -> Result<Self, TrustError> {
		let document = TrustDocument::from_json(bytes)?;
		let keys = document
			.keys
			.iter()
			.map(|entry| TrustedKey::parse(entry))
			.collect::<Result<Vec<_>, _>>()?;
		if keys.is_empty() {
			return Err(TrustError::EmptyTrustRoot);
		}
		Ok(Self {
			version: document.version,
			policy: document.policy,
			keys,
			metadata: document.metadata,
		})
	}
}
