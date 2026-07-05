use crate::text::{as_str, split_message};
use crate::{HashAlgorithm, ObjectError, ObjectId, ObjectKind};

/// A parsed annotated tag object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag<H: HashAlgorithm> {
	/// The object the tag points at.
	pub object: ObjectId<H>,
	/// The kind of the tagged object.
	pub kind: ObjectKind,
	/// The tag name.
	pub name: String,
	/// The raw `tagger` line, if present.
	pub tagger: Option<String>,
	/// The appended armored signature block of a signed tag (`-----BEGIN {PGP,SSH} …`), if
	/// present. Unlike a commit's `gpgsig` *header*, git appends a tag's signature after the
	/// message, so it is stored and re-emitted verbatim (including its trailing newline) — the
	/// message holds everything before it, and [`tag_signed_payload`] reproduces the signed bytes.
	pub signature: Option<String>,
	/// The tag message (up to the appended signature block, if any).
	pub message: String,
}

/// The armored-signature-block markers git recognizes at the start of a line when separating a
/// signed tag's message from its appended signature (mirrors git's `parse_signature`). The
/// earliest line-start occurrence of any of these begins the signature.
const SIGNATURE_MARKERS: [&str; 4] = [
	"-----BEGIN PGP SIGNATURE-----",
	"-----BEGIN PGP MESSAGE-----",
	"-----BEGIN SIGNED MESSAGE-----",
	"-----BEGIN SSH SIGNATURE-----",
];

/// Split a tag's body into `(message, signature)` at the first line beginning a recognized armor
/// block; `signature` is `None` when the body carries no such block. Both parts are verbatim
/// substrings, so concatenating them reproduces the body exactly.
fn split_signature(body: &str) -> (&str, Option<&str>) {
	let mut offset = 0;
	for line in body.split_inclusive('\n') {
		if SIGNATURE_MARKERS
			.iter()
			.any(|marker| line.starts_with(marker))
		{
			return (&body[..offset], Some(&body[offset..]));
		}
		offset += line.len();
	}
	(body, None)
}

/// Parse an annotated tag payload.
pub fn parse_tag<H: HashAlgorithm>(payload: &[u8]) -> Result<Tag<H>, ObjectError> {
	let (header, body) = split_message(payload)?;

	let mut object = None;
	let mut kind = None;
	let mut name = None;
	let mut tagger = None;

	for line in header.split(|&b| b == b'\n') {
		if let Some(rest) = line.strip_prefix(b"object ") {
			object = Some(ObjectId::from_hex(as_str(rest)?)?);
		} else if let Some(rest) = line.strip_prefix(b"type ") {
			kind = Some(ObjectKind::from_wire(rest)?);
		} else if let Some(rest) = line.strip_prefix(b"tag ") {
			name = Some(as_str(rest)?.to_owned());
		} else if let Some(rest) = line.strip_prefix(b"tagger ") {
			tagger = Some(as_str(rest)?.to_owned());
		}
	}

	let (message, signature) = split_signature(body);
	Ok(Tag {
		object: object.ok_or(ObjectError::MalformedHeader)?,
		kind: kind.ok_or(ObjectError::MalformedHeader)?,
		name: name.ok_or(ObjectError::MalformedHeader)?,
		tagger,
		signature: signature.map(str::to_owned),
		message: message.to_owned(),
	})
}

/// Encode an annotated tag to its canonical git payload: `object`, `type`, `tag`, optional
/// `tagger`, blank line, message, then the appended signature block (all emitted verbatim). A
/// signed tag round-trips byte-exact so its id is stable and [`tag_signed_payload`] can reproduce
/// the signed bytes.
pub fn encode_tag<H: HashAlgorithm>(tag: &Tag<H>) -> Vec<u8> {
	let mut out = Vec::new();
	out.extend_from_slice(format!("object {}\n", tag.object).as_bytes());
	out.extend_from_slice(format!("type {}\n", tag.kind.as_str()).as_bytes());
	out.extend_from_slice(format!("tag {}\n", tag.name).as_bytes());
	if let Some(tagger) = &tag.tagger {
		out.extend_from_slice(format!("tagger {tagger}\n").as_bytes());
	}
	out.push(b'\n');
	out.extend_from_slice(tag.message.as_bytes());
	if let Some(signature) = &tag.signature {
		out.extend_from_slice(signature.as_bytes());
	}
	out
}

/// The bytes a signed tag's signature is computed over: the tag re-encoded without its appended
/// signature block. Matches what git signs/verifies.
pub fn tag_signed_payload<H: HashAlgorithm>(tag: &Tag<H>) -> Vec<u8> {
	if tag.signature.is_none() {
		return encode_tag(tag);
	}
	let mut unsigned = tag.clone();
	unsigned.signature = None;
	encode_tag(&unsigned)
}

/// Split a raw tag object buffer into `(signature, signed_payload)`, working on bytes only so a tag
/// with a non-UTF-8 message is handled — [`parse_tag`] would reject it. `signature` is the appended
/// armor block (from its `-----BEGIN … SIGNATURE-----` line to the end), verbatim, or `None` when
/// the tag is unsigned; `signed_payload` is everything before it — exactly the bytes git signs.
pub fn tag_signature_and_payload(raw: &[u8]) -> (Option<Vec<u8>>, Vec<u8>) {
	// The signature can only begin in the body, after the header/message blank line.
	let Some(sep) = raw.windows(2).position(|w| w == b"\n\n") else {
		return (None, raw.to_vec());
	};
	let body_start = sep + 2;
	let mut offset = body_start;
	for line in raw[body_start..].split_inclusive(|&b| b == b'\n') {
		if SIGNATURE_MARKERS
			.iter()
			.any(|marker| line.starts_with(marker.as_bytes()))
		{
			return (Some(raw[offset..].to_vec()), raw[..offset].to_vec());
		}
		offset += line.len();
	}
	(None, raw.to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Sha1, Sha256};

	#[test]
	fn parses_a_tag() {
		let object = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let payload =
			format!("object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n");

		let tag = parse_tag::<Sha256>(payload.as_bytes()).expect("parse");
		assert_eq!(tag.object, object);
		assert_eq!(tag.kind, ObjectKind::Commit);
		assert_eq!(tag.name, "v1");
		assert_eq!(tag.message, "release\n");
	}

	#[test]
	fn encode_round_trips_parse() {
		let object = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let payload =
			format!("object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n");
		let tag = parse_tag::<Sha256>(payload.as_bytes()).expect("parse");
		assert_eq!(tag.signature, None);
		assert_eq!(encode_tag(&tag), payload.as_bytes());
	}

	#[test]
	fn round_trips_a_signed_tag_and_strips_the_signature() {
		// A real `git tag -s` (SSH) object, captured byte-for-byte. The signature block is
		// appended after the message; git verifies everything before the `-----BEGIN` line.
		let payload: &[u8] = b"object e8dec3c943b609a5bf3d030f9a176988fcdb8cc1\ntype commit\ntag v1\ntagger T E St <tagger@example.com> 1700000000 +0000\n\nrelease one\n-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgMhCDdJ9AaIHPx+Gq+KwComelg7\nvE/AY3By/6IdEA0fIAAAADZ2l0AAAAAAAAAAZzaGE1MTIAAABTAAAAC3NzaC1lZDI1NTE5\nAAAAQGjsKHgDSUI1RzUEINOZuHpz171e23pl30v85ALlcU2Kb1yWcY1DBkoSL0uujy1R81\nZ5Ba/1qf6az+khkFE8LQM=\n-----END SSH SIGNATURE-----\n";
		let tag = parse_tag::<Sha1>(payload).expect("parse");

		// The signature is split out of the message, verbatim.
		let signature = tag.signature.clone().expect("has a signature");
		assert!(signature.starts_with("-----BEGIN SSH SIGNATURE-----"));
		assert!(signature.ends_with("-----END SSH SIGNATURE-----\n"));
		assert_eq!(tag.message, "release one\n");

		// Encoding is byte-exact (so the signed tag's id is stable).
		assert_eq!(encode_tag(&tag), payload);

		// The signed payload drops the appended block and equals everything git signs: the bytes
		// up to the `-----BEGIN` marker (offset 132 in this fixture).
		let signed = tag_signed_payload(&tag);
		assert_eq!(signed, &payload[..132]);
		let reparsed = parse_tag::<Sha1>(&signed).expect("reparse signed payload");
		assert_eq!(reparsed.signature, None);
		assert_eq!(reparsed.message, "release one\n");
		assert_eq!(reparsed.object, tag.object);
	}

	#[test]
	fn signature_and_payload_from_bytes_splits_at_the_appended_block() {
		let object = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let payload = format!(
			"object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n\
			 -----BEGIN SSH SIGNATURE-----\nabc\n-----END SSH SIGNATURE-----\n"
		);
		let (signature, signed) = tag_signature_and_payload(payload.as_bytes());
		assert_eq!(
			signature.as_deref(),
			Some(b"-----BEGIN SSH SIGNATURE-----\nabc\n-----END SSH SIGNATURE-----\n".as_slice())
		);
		// The signed payload is byte-identical to the struct path for a well-formed tag.
		let tag = parse_tag::<Sha256>(payload.as_bytes()).expect("parse");
		assert_eq!(signed, tag_signed_payload(&tag));
	}

	#[test]
	fn signature_and_payload_from_bytes_reports_an_unsigned_tag() {
		let object = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let payload =
			format!("object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n");
		let (signature, signed) = tag_signature_and_payload(payload.as_bytes());
		assert_eq!(signature, None);
		assert_eq!(signed, payload.as_bytes());
	}

	#[test]
	fn recognizes_a_pgp_signature_block() {
		// The boundary logic is armor-agnostic: a PGP block splits just like an SSH one.
		let object = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let payload = format!(
			"object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nbody\n\
			 -----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----\n"
		);
		let tag = parse_tag::<Sha256>(payload.as_bytes()).expect("parse");
		assert_eq!(tag.message, "body\n");
		assert!(
			tag
				.signature
				.as_deref()
				.is_some_and(|s| s.starts_with("-----BEGIN PGP SIGNATURE-----"))
		);
		assert_eq!(encode_tag(&tag), payload.as_bytes());
	}
}
