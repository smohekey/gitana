use crate::text::{as_str, split_message};
use crate::{ObjectError, ObjectId, ObjectKind};

/// A parsed annotated tag object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
	/// The object the tag points at.
	pub object: ObjectId,
	/// The kind of the tagged object.
	pub kind: ObjectKind,
	/// The tag name.
	pub name: String,
	/// The raw `tagger` line, if present.
	pub tagger: Option<String>,
	/// The tag message.
	pub message: String,
}

/// Parse an annotated tag payload.
pub fn parse_tag(payload: &[u8]) -> Result<Tag, ObjectError> {
	let (header, message) = split_message(payload)?;

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

	Ok(Tag {
		object: object.ok_or(ObjectError::MalformedHeader)?,
		kind: kind.ok_or(ObjectError::MalformedHeader)?,
		name: name.ok_or(ObjectError::MalformedHeader)?,
		tagger,
		message: message.to_owned(),
	})
}

/// Encode an annotated tag to its canonical git payload: `object`, `type`, `tag`,
/// optional `tagger`, blank line, message (emitted verbatim).
pub fn encode_tag(tag: &Tag) -> Vec<u8> {
	let mut out = Vec::new();
	out.extend_from_slice(format!("object {}\n", tag.object).as_bytes());
	out.extend_from_slice(format!("type {}\n", tag.kind.as_str()).as_bytes());
	out.extend_from_slice(format!("tag {}\n", tag.name).as_bytes());
	if let Some(tagger) = &tag.tagger {
		out.extend_from_slice(format!("tagger {tagger}\n").as_bytes());
	}
	out.push(b'\n');
	out.extend_from_slice(tag.message.as_bytes());
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_a_tag() {
		let object = ObjectId::compute(ObjectKind::Commit, b"c");
		let payload =
			format!("object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n");

		let tag = parse_tag(payload.as_bytes()).expect("parse");
		assert_eq!(tag.object, object);
		assert_eq!(tag.kind, ObjectKind::Commit);
		assert_eq!(tag.name, "v1");
		assert_eq!(tag.message, "release\n");
	}

	#[test]
	fn encode_round_trips_parse() {
		let object = ObjectId::compute(ObjectKind::Commit, b"c");
		let payload =
			format!("object {object}\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\nrelease\n");
		let tag = parse_tag(payload.as_bytes()).expect("parse");
		assert_eq!(encode_tag(&tag), payload.as_bytes());
	}
}
