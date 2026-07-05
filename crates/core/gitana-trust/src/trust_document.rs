use crate::{Policy, TrustError};

/// The on-disk JSON shape of a trust document: the editable, human-authored representation stored at
/// `trust.json` in a trust commit's tree. Keys are OpenSSH public-key lines
/// (`ssh-ed25519 AAAA… comment`), kept verbatim so a document round-trips byte-stable.
///
/// This is the *writable* counterpart to [`crate::TrustRoot`]: the CLI reads a document, edits its
/// key list or policy, and serialises it back, while [`crate::TrustRoot`] is the *verified, folded*
/// form (keys parsed) that the enforcement path consumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TrustDocument {
	/// The document schema version.
	pub version: u32,
	/// The enforcement policy (see [`Policy`]).
	pub policy: Policy,
	/// The trusted signing keys, as OpenSSH public-key lines.
	pub keys: Vec<String>,
	/// Free-form document metadata, preserved but not interpreted in v1. Omitted from the serialised
	/// form when null, so a minimal document stays minimal.
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub metadata: serde_json::Value,
}

impl TrustDocument {
	/// A new document enrolling `keys` (OpenSSH public-key lines) under `policy`, with null metadata.
	pub fn new(version: u32, policy: Policy, keys: Vec<String>) -> Self {
		Self {
			version,
			policy,
			keys,
			metadata: serde_json::Value::Null,
		}
	}

	/// Parse a trust document from its canonical JSON bytes. Errors only on malformed JSON — key
	/// lines are validated when the document is folded into a [`crate::TrustRoot`], not here.
	pub fn from_json(bytes: &[u8]) -> Result<Self, TrustError> {
		serde_json::from_slice(bytes).map_err(TrustError::MalformedTrustDocument)
	}

	/// Serialise to canonical JSON: 2-space-indented, fields in declaration order, with a trailing
	/// newline. Deterministic, so re-writing an unchanged document yields identical bytes (and thus a
	/// stable blob id). Serialisation of this fixed shape is infallible.
	pub fn to_json(&self) -> Vec<u8> {
		let mut bytes = serde_json::to_vec_pretty(self).expect("trust document serialises");
		bytes.push(b'\n');
		bytes
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample admin@example.com";

	#[test]
	fn round_trips_through_json_byte_stable() {
		let doc = TrustDocument::new(1, Policy::Warn, vec![KEY.to_owned()]);
		let json = doc.to_json();
		// Reparses to the same document, and re-serialising is byte-identical (a stable blob id).
		let reparsed = TrustDocument::from_json(&json).unwrap();
		assert_eq!(reparsed, doc);
		assert_eq!(reparsed.to_json(), json);
	}

	#[test]
	fn omits_null_metadata_but_preserves_present_metadata() {
		let minimal = TrustDocument::new(1, Policy::Require, vec![KEY.to_owned()]);
		let text = String::from_utf8(minimal.to_json()).unwrap();
		assert!(
			!text.contains("metadata"),
			"null metadata is not serialised: {text}"
		);

		let mut annotated = minimal.clone();
		annotated.metadata = serde_json::json!({ "note": "founding root" });
		let reparsed = TrustDocument::from_json(&annotated.to_json()).unwrap();
		assert_eq!(reparsed.metadata, annotated.metadata);
	}
}
