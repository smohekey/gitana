//! Property/fuzz tests for trust-document / trust-root JSON parsing
//! (`docs/hlds/secure-git-trust-signing.md`, step 8 validation plan): parsing arbitrary bytes must
//! never panic (only `Err`), and a trust document survives a JSON round-trip.

use gitana_trust::{Policy, TrustDocument, TrustRoot};
use proptest::prelude::*;

fn arb_policy() -> impl Strategy<Value = Policy> {
	prop_oneof![Just(Policy::Off), Just(Policy::Warn), Just(Policy::Require),]
}

/// A trust document with an arbitrary version, policy, and key lines (key contents are not validated
/// at the document layer, only when folded into a [`TrustRoot`], so any printable string is fine).
fn arb_document() -> impl Strategy<Value = TrustDocument> {
	(
		any::<u32>(),
		arb_policy(),
		proptest::collection::vec("[ -~]{0,60}", 0..5),
	)
		.prop_map(|(version, policy, keys)| TrustDocument::new(version, policy, keys))
}

proptest! {
	#[test]
	fn trust_root_from_json_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = TrustRoot::from_json(&bytes);
	}

	#[test]
	fn trust_document_from_json_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = TrustDocument::from_json(&bytes);
	}

	#[test]
	fn trust_document_json_roundtrips(doc in arb_document()) {
		let reparsed = TrustDocument::from_json(&doc.to_json()).expect("re-parse");
		prop_assert_eq!(reparsed, doc);
	}
}
