use crate::text::{as_str, split_message};
use crate::{HashAlgorithm, ObjectError, ObjectId, Signature};

/// A parsed commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit<H: HashAlgorithm> {
	/// The tree this commit points at.
	pub tree: ObjectId<H>,
	/// Parent commits (zero for a root commit, one normally, more for merges).
	pub parents: Vec<ObjectId<H>>,
	/// The raw `author` line (name, email, time).
	pub author: String,
	/// The raw `committer` line.
	pub committer: String,
	/// The `gpgsig` signature block (an `SSHSIG`/PGP armored signature), if signed. The
	/// value with git's multi-line continuation spaces stripped (lines joined by `\n`).
	pub signature: Option<String>,
	/// Header lines other than `tree`/`parent`/`author`/`committer`/`gpgsig`, in order — e.g.
	/// `encoding` and `mergetag` (a merge of a signed tag embeds the tag here). Each is `(name,
	/// value)` with the value unfolded (git's space-prefixed continuation lines joined by `\n`) and
	/// held as raw bytes, since a `mergetag`'s embedded tag can carry a non-UTF-8 message. Preserved
	/// so [`encode_commit`] reproduces the commit byte-for-byte (and its id is stable).
	pub extra_headers: Vec<(String, Vec<u8>)>,
	/// The commit message (everything after the header's blank line).
	pub message: String,
}

/// Parse a commit payload.
pub fn parse_commit<H: HashAlgorithm>(payload: &[u8]) -> Result<Commit<H>, ObjectError> {
	let (header, message) = split_message(payload)?;

	let mut tree = None;
	let mut parents = Vec::new();
	let mut author = None;
	let mut committer = None;
	let mut signature = None;
	let mut extra_headers = Vec::new();

	let gpgsig_prefix = format!("{} ", H::GPGSIG_HEADER);
	let mut lines = header.split(|&b| b == b'\n').peekable();
	while let Some(line) = lines.next() {
		if let Some(rest) = line.strip_prefix(b"tree ") {
			tree = Some(ObjectId::from_hex(as_str(rest)?)?);
		} else if let Some(rest) = line.strip_prefix(b"parent ") {
			parents.push(ObjectId::from_hex(as_str(rest)?)?);
		} else if let Some(rest) = line.strip_prefix(b"author ") {
			author = Some(as_str(rest)?.to_owned());
		} else if let Some(rest) = line.strip_prefix(b"committer ") {
			committer = Some(as_str(rest)?.to_owned());
		} else if let Some(rest) = line.strip_prefix(gpgsig_prefix.as_bytes()) {
			signature = Some(as_str(&unfold(rest, &mut lines))?.to_owned());
		} else if !line.is_empty() {
			// Any other header (e.g. `encoding`, `mergetag`), preserved verbatim so re-encoding is
			// byte-exact. Split `name value`; the value keeps its raw bytes (a `mergetag` embeds a
			// tag whose message may not be UTF-8).
			let split = line.iter().position(|&b| b == b' ');
			let (name, first) = split.map_or((line, &b""[..]), |i| (&line[..i], &line[i + 1..]));
			extra_headers.push((as_str(name)?.to_owned(), unfold(first, &mut lines)));
		}
	}

	Ok(Commit {
		tree: tree.ok_or(ObjectError::MalformedHeader)?,
		parents,
		author: author.ok_or(ObjectError::MalformedHeader)?,
		committer: committer.ok_or(ObjectError::MalformedHeader)?,
		signature,
		extra_headers,
		message: message.to_owned(),
	})
}

/// Validate the raw structural encoding of a git commit.
///
/// Valid commits contain one leading `tree` header, zero or more contiguous
/// `parent` headers, exactly one `author`, and exactly one `committer`, in that
/// order. Remaining headers must have valid names and continuation lines, and
/// the hash algorithm's signature header may occur at most once. Author and
/// committer values must be canonical git identity lines with non-negative
/// timestamps.
///
/// This validation is intentionally byte-oriented and does not require the
/// commit message or extra-header values to be UTF-8.
pub fn validate_commit_structure<H: HashAlgorithm>(payload: &[u8]) -> Result<(), ObjectError> {
	let separator = payload
		.windows(2)
		.position(|bytes| bytes == b"\n\n")
		.ok_or(ObjectError::InvalidCommitStructure)?;
	let header = &payload[..separator];
	if header.is_empty() || header.contains(&0) {
		return Err(ObjectError::InvalidCommitStructure);
	}

	let mut lines = header.split(|byte| *byte == b'\n').peekable();
	validate_object_id_header::<H>(
		lines.next().ok_or(ObjectError::InvalidCommitStructure)?,
		b"tree ",
	)?;
	while lines
		.peek()
		.is_some_and(|line| line.starts_with(b"parent "))
	{
		validate_object_id_header::<H>(lines.next().expect("peeked parent header"), b"parent ")?;
	}
	validate_identity_header(
		lines.next().ok_or(ObjectError::InvalidCommitStructure)?,
		b"author ",
	)?;
	validate_identity_header(
		lines.next().ok_or(ObjectError::InvalidCommitStructure)?,
		b"committer ",
	)?;

	let signature_name = H::GPGSIG_HEADER.as_bytes();
	let mut signature_seen = false;
	let mut continuation_allowed = false;
	for line in lines {
		if line.starts_with(b" ") {
			if !continuation_allowed {
				return Err(ObjectError::InvalidCommitStructure);
			}
			continue;
		}
		let space = line
			.iter()
			.position(|byte| *byte == b' ')
			.ok_or(ObjectError::InvalidCommitStructure)?;
		let name = &line[..space];
		if name.is_empty()
			|| !name
				.iter()
				.all(|byte| byte.is_ascii_graphic() && *byte != b' ')
			|| matches!(name, b"tree" | b"parent" | b"author" | b"committer")
		{
			return Err(ObjectError::InvalidCommitStructure);
		}
		if name == signature_name {
			if signature_seen {
				return Err(ObjectError::InvalidCommitStructure);
			}
			signature_seen = true;
		}
		continuation_allowed = true;
	}
	Ok(())
}

fn validate_object_id_header<H: HashAlgorithm>(
	line: &[u8],
	prefix: &[u8],
) -> Result<(), ObjectError> {
	let value = line
		.strip_prefix(prefix)
		.ok_or(ObjectError::InvalidCommitStructure)?;
	let value = as_str(value).map_err(|_| ObjectError::InvalidCommitStructure)?;
	ObjectId::<H>::from_hex(value).map_err(|_| ObjectError::InvalidCommitStructure)?;
	Ok(())
}

fn validate_identity_header(line: &[u8], prefix: &[u8]) -> Result<(), ObjectError> {
	let value = line
		.strip_prefix(prefix)
		.ok_or(ObjectError::InvalidCommitStructure)?;
	let value = as_str(value).map_err(|_| ObjectError::InvalidCommitIdentity)?;
	let identity = Signature::parse(value).map_err(|_| ObjectError::InvalidCommitIdentity)?;
	if identity.name.is_empty()
		|| identity.email.is_empty()
		|| identity.seconds < 0
		|| identity.to_string() != value
	{
		return Err(ObjectError::InvalidCommitIdentity);
	}
	Ok(())
}

/// Unfold a multi-line header value: `first` (the bytes after `name `) plus each following
/// space-prefixed continuation line (its leading space removed), joined by `\n`. Consumes the
/// continuation lines from `lines`. Mirrors [`encode_commit`]'s folding, so the round-trip is exact.
fn unfold<'a>(
	first: &[u8],
	lines: &mut std::iter::Peekable<impl Iterator<Item = &'a [u8]>>,
) -> Vec<u8> {
	let mut value = first.to_vec();
	while let Some(continuation) = lines.peek().and_then(|next| next.strip_prefix(b" ")) {
		value.push(b'\n');
		value.extend_from_slice(continuation);
		lines.next();
	}
	value
}

/// Encode a commit to its canonical git payload: `tree`, `parent`*, `author`, `committer`, any
/// [`Commit::extra_headers`] (e.g. `encoding`, `mergetag`), optional `gpgsig`, blank line, message.
/// Byte-exact with git, so any commit round-trips (its id is stable) and [`commit_signed_payload`]
/// reproduces the signed bytes.
pub fn encode_commit<H: HashAlgorithm>(commit: &Commit<H>) -> Vec<u8> {
	let mut out = Vec::new();
	out.extend_from_slice(format!("tree {}\n", commit.tree).as_bytes());
	for parent in &commit.parents {
		out.extend_from_slice(format!("parent {parent}\n").as_bytes());
	}
	out.extend_from_slice(format!("author {}\n", commit.author).as_bytes());
	out.extend_from_slice(format!("committer {}\n", commit.committer).as_bytes());
	for (name, value) in &commit.extra_headers {
		// `name <first line>`, then each unfolded continuation re-folded with a leading space. git
		// writes these (e.g. `encoding`, `mergetag`) between `committer` and `gpgsig`.
		let mut segments = value.split(|&b| b == b'\n');
		out.extend_from_slice(name.as_bytes());
		out.push(b' ');
		out.extend_from_slice(segments.next().unwrap_or_default());
		out.push(b'\n');
		for segment in segments {
			out.push(b' ');
			out.extend_from_slice(segment);
			out.push(b'\n');
		}
	}
	if let Some(signature) = &commit.signature {
		// First line trails `<gpgsig header> `; continuations a single space (git's format).
		let mut signature_lines = signature.split('\n');
		if let Some(first) = signature_lines.next() {
			out.extend_from_slice(format!("{} {first}\n", H::GPGSIG_HEADER).as_bytes());
		}
		for line in signature_lines {
			out.extend_from_slice(format!(" {line}\n").as_bytes());
		}
	}
	out.push(b'\n');
	out.extend_from_slice(commit.message.as_bytes());
	out
}

/// The bytes a signed commit's signature is computed over: the commit re-encoded
/// without its `gpgsig` header. Matches what git signs/verifies.
pub fn commit_signed_payload<H: HashAlgorithm>(commit: &Commit<H>) -> Vec<u8> {
	if commit.signature.is_none() {
		return encode_commit(commit);
	}
	let mut unsigned = commit.clone();
	unsigned.signature = None;
	encode_commit(&unsigned)
}

/// Split a raw commit object buffer into `(signature, signed_payload)`, working on bytes only so a
/// commit with a non-UTF-8 message (git's `encoding` header) is handled — [`parse_commit`] would
/// reject it. `signature` is the unfolded `gpgsig` armor (its header value with the space-prefixed
/// continuation lines joined by `\n`), or `None` when the commit is unsigned. `signed_payload` is
/// `raw` with the `gpgsig` header removed and every other byte — including headers the parser does
/// not model, such as `mergetag` and `encoding` — left intact: exactly the bytes git signs. Unlike
/// [`commit_signed_payload`], this reproduces git's signed bytes for a commit carrying extra
/// headers (e.g. a merge of a signed tag), which the lossy parse/encode round-trip cannot.
pub fn commit_signature_and_payload<H: HashAlgorithm>(raw: &[u8]) -> (Option<Vec<u8>>, Vec<u8>) {
	let gpgsig_prefix = format!("{} ", H::GPGSIG_HEADER);
	let mut signature = None;
	let mut payload = Vec::with_capacity(raw.len());
	// Only the header region (before the blank separator) can hold a `gpgsig` header; once into the
	// message, copy verbatim so a message line that happens to start with `gpgsig ` is untouched.
	let mut in_message = false;
	let mut lines = raw.split_inclusive(|&b| b == b'\n').peekable();
	while let Some(line) = lines.next() {
		if !in_message && line.starts_with(gpgsig_prefix.as_bytes()) {
			// Collect the armor (this line's value) and its continuation lines (each begins with a
			// single space), dropping the whole block from the payload. `armor` joins them with `\n`,
			// matching what `parse_commit` produces and what `SshSig::from_pem` expects.
			let mut armor = trim_newline(&line[gpgsig_prefix.len()..]).to_vec();
			while let Some(rest) = lines.peek().and_then(|next| next.strip_prefix(b" ")) {
				armor.push(b'\n');
				armor.extend_from_slice(trim_newline(rest));
				lines.next();
			}
			signature = Some(armor);
			continue;
		}
		// A bare newline (or trailing empty slice) ends the header block.
		if line == b"\n" || line.is_empty() {
			in_message = true;
		}
		payload.extend_from_slice(line);
	}
	(signature, payload)
}

/// Drop a single trailing `\n` (as `split_inclusive` leaves on every line but the last).
fn trim_newline(line: &[u8]) -> &[u8] {
	line.strip_suffix(b"\n").unwrap_or(line)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ObjectKind, Sha256};

	#[test]
	fn parses_a_merge_commit() {
		let tree = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"t");
		let p1 = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let p2 = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");
		let payload = format!(
			"tree {tree}\nparent {p1}\nparent {p2}\n\
             author A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\n\nmerge\n",
		);

		let commit = parse_commit::<Sha256>(payload.as_bytes()).expect("parse");
		assert_eq!(commit.tree, tree);
		assert_eq!(commit.parents, vec![p1, p2]);
		assert_eq!(commit.author, "A <a@x> 1 +0000");
		assert_eq!(commit.message, "merge\n");
	}

	#[test]
	fn validates_raw_commit_structure() {
		let tree = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"t");
		let parent = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"p");
		let payload = format!(
			"tree {tree}\nparent {parent}\n\
			 author A <a@x> 1 +0000\ncommitter C <c@x> 2 -0000\n\
			 encoding UTF-8\ncustom first\n continuation\n\nmessage\n",
		);
		validate_commit_structure::<Sha256>(payload.as_bytes()).expect("valid commit structure");
	}

	#[test]
	fn rejects_duplicate_or_misordered_core_headers() {
		let tree = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"t");
		for payload in [
			format!(
				"tree {tree}\ntree {tree}\nauthor A <a@x> 1 +0000\n\
				 committer C <c@x> 2 +0000\n\nmessage\n"
			),
			format!(
				"tree {tree}\nauthor A <a@x> 1 +0000\nauthor B <b@x> 1 +0000\n\
				 committer C <c@x> 2 +0000\n\nmessage\n"
			),
			format!(
				"tree {tree}\nencoding UTF-8\nauthor A <a@x> 1 +0000\n\
				 committer C <c@x> 2 +0000\n\nmessage\n"
			),
		] {
			assert!(matches!(
				validate_commit_structure::<Sha256>(payload.as_bytes()),
				Err(ObjectError::InvalidCommitStructure)
			));
		}
	}

	#[test]
	fn rejects_invalid_commit_identities() {
		let tree = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"t");
		for author in [
			"A <a@x> -1 +0000",
			"A <a@x> 01 +0000",
			"A <a@x> 1 +0000 trailing",
			" <a@x> 1 +0000",
		] {
			let payload = format!("tree {tree}\nauthor {author}\ncommitter C <c@x> 2 +0000\n\nmessage\n");
			assert!(matches!(
				validate_commit_structure::<Sha256>(payload.as_bytes()),
				Err(ObjectError::InvalidCommitIdentity)
			));
		}
	}

	#[test]
	fn commit_matches_git_sha256() {
		// Fixture from `git commit-tree` (sha256) with fixed author/committer dates.
		let tree = ObjectId::<Sha256>::from_hex(
			"b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804",
		)
		.unwrap();
		let commit = Commit {
			tree,
			parents: vec![],
			author: "A U Thor <author@example.com> 1700000000 +1000".to_owned(),
			committer: "C O Mitter <committer@example.com> 1700000005 -0500".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "first commit\n".to_owned(),
		};
		let id = ObjectId::<Sha256>::compute(ObjectKind::Commit, &encode_commit(&commit));
		assert_eq!(
			id.to_hex(),
			"a2dd0047ccdabef362d8a41ee931f28847d11073a75c7eb3cee9028d03b017df"
		);
	}

	#[test]
	fn encode_round_trips_parse() {
		let payload = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\n\
			author A U Thor <author@example.com> 1700000000 +1000\n\
			committer C O Mitter <committer@example.com> 1700000005 -0500\n\n\
			first commit\n";
		let commit = parse_commit::<Sha256>(payload).expect("parse");
		assert_eq!(encode_commit(&commit), payload);
	}

	#[test]
	fn round_trips_a_signed_commit_and_strips_the_signature() {
		// A gpgsig header (multi-line, continuation lines start with a space), as git
		// writes it. The signature bytes here are illustrative, not a real signature.
		// Written on one line (with explicit `\n `) so the continuation spaces survive.
		let payload: &[u8] = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\nauthor A U Thor <author@example.com> 1700000000 +1000\ncommitter C O Mitter <committer@example.com> 1700000005 -0500\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAg\n AAAABHNoYTUxMgAAAFMAAAALc3NoLWVkMjU1MTkAAABA\n -----END SSH SIGNATURE-----\n\nsigned commit\n";
		let commit = parse_commit::<Sha256>(payload).expect("parse");

		let signature = commit.signature.clone().expect("has a signature");
		assert!(signature.starts_with("-----BEGIN SSH SIGNATURE-----"));
		assert!(signature.ends_with("-----END SSH SIGNATURE-----"));
		assert!(signature.contains('\n'), "multi-line block preserved");

		// Encoding is byte-exact (so the signed commit's id is stable).
		assert_eq!(encode_commit(&commit), payload);

		// The signed payload drops the gpgsig header but is otherwise the same commit.
		let signed = commit_signed_payload(&commit);
		assert!(
			!signed.windows(6).any(|w| w == b"gpgsig"),
			"no gpgsig in payload"
		);
		let reparsed = parse_commit::<Sha256>(&signed).expect("reparse signed payload");
		assert_eq!(reparsed.signature, None);
		assert_eq!(reparsed.tree, commit.tree);
		assert_eq!(reparsed.message, commit.message);
	}

	#[test]
	fn round_trips_encoding_and_mergetag_headers_byte_exact() {
		// A signed merge of a signed tag: `encoding`, a multi-line `mergetag` (whose embedded tag
		// message carries a non-UTF-8 byte, `\xe9`), then `gpgsig` — git's header order. The parser
		// dropped these headers before, so re-encoding changed the id; now it must round-trip.
		let payload: &[u8] = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\nparent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nparent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nauthor A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\nencoding ISO-8859-1\nmergetag object bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n type commit\n tag v1\n tagger T <t@x> 1 +0000\n \n rel\xe9ase\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\n\nmerge\n";
		let commit = parse_commit::<Sha256>(payload).expect("parse");

		// Both headers preserved, in order, with raw (non-UTF-8-safe) values.
		assert_eq!(commit.extra_headers.len(), 2);
		assert_eq!(commit.extra_headers[0].0, "encoding");
		assert_eq!(commit.extra_headers[0].1, b"ISO-8859-1");
		assert_eq!(commit.extra_headers[1].0, "mergetag");
		assert!(
			commit.extra_headers[1].1.contains(&0xe9),
			"non-UTF-8 mergetag byte preserved"
		);
		assert!(commit.signature.is_some(), "gpgsig still parsed");

		// The whole commit round-trips byte-for-byte, so its id is stable.
		assert_eq!(encode_commit(&commit), payload);
	}

	#[test]
	fn signature_and_payload_from_bytes_matches_the_struct_path_for_a_simple_commit() {
		let payload: &[u8] = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\nauthor A U Thor <author@example.com> 1700000000 +1000\ncommitter C O Mitter <committer@example.com> 1700000005 -0500\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAg\n AAAABHNoYTUxMgAAAFMAAAALc3NoLWVkMjU1MTkAAABA\n -----END SSH SIGNATURE-----\n\nsigned commit\n";
		let commit = parse_commit::<Sha256>(payload).expect("parse");
		let (signature, signed) = commit_signature_and_payload::<Sha256>(payload);
		assert_eq!(signed, commit_signed_payload(&commit));
		// The extracted armor matches the unfolded gpgsig value the parser recovers.
		assert_eq!(
			signature.map(|s| String::from_utf8(s).unwrap()),
			commit.signature
		);
	}

	#[test]
	fn signature_and_payload_from_bytes_keeps_headers_the_parser_drops() {
		// A merge commit whose `mergetag` header (multi-line) precedes `gpgsig` — git signs the
		// mergetag too, but `parse_commit` drops it, so only the byte path reproduces the payload.
		let payload: &[u8] = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\nparent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nparent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nauthor A U Thor <a@example.com> 1700000000 +0000\ncommitter C O Mitter <c@example.com> 1700000000 +0000\nmergetag object bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n type commit\n tag v1\n tagger T <t@example.com> 1700000000 +0000\n \n release\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\n\nmerge\n";
		let (signature, signed) = commit_signature_and_payload::<Sha256>(payload);
		assert_eq!(
			signature.as_deref(),
			Some(b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----".as_slice())
		);
		// The gpgsig block is gone, the mergetag block survives byte-for-byte, and the payload is
		// exactly the input with only the gpgsig header excised.
		assert!(!signed.windows(6).any(|w| w == b"gpgsig"), "gpgsig removed");
		let marker = b"mergetag object";
		assert!(
			signed.windows(marker.len()).any(|w| w == marker),
			"mergetag preserved"
		);
		let gpgsig_start = payload
			.windows(7)
			.position(|w| w == b"gpgsig-")
			.expect("has gpgsig");
		let mut expected = payload[..gpgsig_start].to_vec();
		expected.extend_from_slice(b"\nmerge\n");
		assert_eq!(signed, expected);
	}
}
