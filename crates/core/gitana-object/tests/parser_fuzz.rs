//! Property/fuzz tests for the object parsers (`docs/hlds/secure-git-trust-signing.md`, step 8
//! validation plan). Two invariants:
//!
//! - **No panic on arbitrary bytes.** A malformed pushed object or pkt-line must yield `Err`, never
//!   crash the receive path. `proptest` shrinks any panic to a minimal input.
//! - **Encode is a left inverse of parse.** A structurally-valid commit/tag survives an
//!   encode→parse round-trip. Byte-exact round-tripping of *real signed* objects (gpgsig folding,
//!   extra headers, mergetag, non-utf8 encodings) is covered by the crate's fixture tests; these
//!   generators exercise the structural core (no signature / extra headers).

use gitana_object::{
	Commit, ObjectId, ObjectKind, Sha256, Tag, commit_signature_and_payload, encode_commit,
	encode_tag, parse_commit, parse_pkt, parse_tag, tag_signature_and_payload,
};
use proptest::prelude::*;

/// A well-formed object id derived from arbitrary bytes (value is irrelevant; only the shape is).
fn arb_oid() -> impl Strategy<Value = ObjectId<Sha256>> {
	proptest::collection::vec(any::<u8>(), 0..8)
		.prop_map(|bytes| ObjectId::compute(ObjectKind::Blob, &bytes))
}

fn arb_kind() -> impl Strategy<Value = ObjectKind> {
	prop_oneof![
		Just(ObjectKind::Commit),
		Just(ObjectKind::Tree),
		Just(ObjectKind::Blob),
		Just(ObjectKind::Tag),
	]
}

fn arb_commit() -> impl Strategy<Value = Commit<Sha256>> {
	(
		arb_oid(),
		proptest::collection::vec(arb_oid(), 0..3),
		"[a-zA-Z0-9 <>@.+_-]{1,40}",
		"[a-zA-Z0-9 <>@.+_-]{1,40}",
		"[a-zA-Z0-9 \n]{0,80}",
	)
		.prop_map(|(tree, parents, author, committer, message)| Commit {
			tree,
			parents,
			author,
			committer,
			signature: None,
			extra_headers: Vec::new(),
			message,
		})
}

fn arb_tag() -> impl Strategy<Value = Tag<Sha256>> {
	(
		arb_oid(),
		arb_kind(),
		"[a-zA-Z0-9.+_-]{1,40}",
		proptest::option::of("[a-zA-Z0-9 <>@.+_-]{1,40}"),
		"[a-zA-Z0-9 \n]{0,80}",
	)
		.prop_map(|(object, kind, name, tagger, message)| Tag {
			object,
			kind,
			name,
			tagger,
			signature: None,
			message,
		})
}

proptest! {
	#[test]
	fn parse_pkt_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = parse_pkt(&bytes);
	}

	#[test]
	fn parse_commit_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = parse_commit::<Sha256>(&bytes);
	}

	#[test]
	fn parse_tag_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = parse_tag::<Sha256>(&bytes);
	}

	#[test]
	fn commit_signature_split_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = commit_signature_and_payload::<Sha256>(&bytes);
	}

	#[test]
	fn tag_signature_split_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
		let _ = tag_signature_and_payload(&bytes);
	}

	#[test]
	fn commit_encode_parse_roundtrips(commit in arb_commit()) {
		let reparsed = parse_commit::<Sha256>(&encode_commit(&commit)).expect("re-parse");
		prop_assert_eq!(reparsed, commit);
	}

	#[test]
	fn tag_encode_parse_roundtrips(tag in arb_tag()) {
		let reparsed = parse_tag::<Sha256>(&encode_tag(&tag)).expect("re-parse");
		prop_assert_eq!(reparsed, tag);
	}
}
