use crate::text::{as_str, split_message};
use crate::{ObjectError, ObjectId};

/// The commit signature header. SHA-256 repositories use `gpgsig-sha256` (the bare
/// `gpgsig` header carries the SHA-1 object's signature, which gitana never writes).
const GPGSIG_HEADER: &str = "gpgsig-sha256";

/// A parsed commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
	/// The tree this commit points at.
	pub tree: ObjectId,
	/// Parent commits (zero for a root commit, one normally, more for merges).
	pub parents: Vec<ObjectId>,
	/// The raw `author` line (name, email, time).
	pub author: String,
	/// The raw `committer` line.
	pub committer: String,
	/// The `gpgsig` signature block (an `SSHSIG`/PGP armored signature), if signed. The
	/// value with git's multi-line continuation spaces stripped (lines joined by `\n`).
	pub signature: Option<String>,
	/// The commit message (everything after the header's blank line).
	pub message: String,
}

/// Parse a commit payload.
pub fn parse_commit(payload: &[u8]) -> Result<Commit, ObjectError> {
	let (header, message) = split_message(payload)?;

	let mut tree = None;
	let mut parents = Vec::new();
	let mut author = None;
	let mut committer = None;
	let mut signature = None;

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
		} else if let Some(rest) = line.strip_prefix(format!("{GPGSIG_HEADER} ").as_bytes()) {
			// A multi-line header: the value continues on lines starting with a space.
			let mut value = as_str(rest)?.to_owned();
			while let Some(next) = lines.peek() {
				let Some(continuation) = next.strip_prefix(b" ") else {
					break;
				};
				value.push('\n');
				value.push_str(as_str(continuation)?);
				lines.next();
			}
			signature = Some(value);
		}
	}

	Ok(Commit {
		tree: tree.ok_or(ObjectError::MalformedHeader)?,
		parents,
		author: author.ok_or(ObjectError::MalformedHeader)?,
		committer: committer.ok_or(ObjectError::MalformedHeader)?,
		signature,
		message: message.to_owned(),
	})
}

/// Encode a commit to its canonical git payload: `tree`, `parent`*, `author`,
/// `committer`, optional `gpgsig`, blank line, message. Byte-exact with git, so a
/// signed commit round-trips and [`commit_signed_payload`] reproduces the signed bytes.
pub fn encode_commit(commit: &Commit) -> Vec<u8> {
	let mut out = Vec::new();
	out.extend_from_slice(format!("tree {}\n", commit.tree).as_bytes());
	for parent in &commit.parents {
		out.extend_from_slice(format!("parent {parent}\n").as_bytes());
	}
	out.extend_from_slice(format!("author {}\n", commit.author).as_bytes());
	out.extend_from_slice(format!("committer {}\n", commit.committer).as_bytes());
	if let Some(signature) = &commit.signature {
		// First line trails `gpgsig-sha256 `; continuations a single space (git's format).
		let mut signature_lines = signature.split('\n');
		if let Some(first) = signature_lines.next() {
			out.extend_from_slice(format!("{GPGSIG_HEADER} {first}\n").as_bytes());
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
pub fn commit_signed_payload(commit: &Commit) -> Vec<u8> {
	if commit.signature.is_none() {
		return encode_commit(commit);
	}
	let mut unsigned = commit.clone();
	unsigned.signature = None;
	encode_commit(&unsigned)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ObjectKind;

	#[test]
	fn parses_a_merge_commit() {
		let tree = ObjectId::compute(ObjectKind::Tree, b"t");
		let p1 = ObjectId::compute(ObjectKind::Commit, b"a");
		let p2 = ObjectId::compute(ObjectKind::Commit, b"b");
		let payload = format!(
			"tree {tree}\nparent {p1}\nparent {p2}\n\
             author A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\n\nmerge\n",
		);

		let commit = parse_commit(payload.as_bytes()).expect("parse");
		assert_eq!(commit.tree, tree);
		assert_eq!(commit.parents, vec![p1, p2]);
		assert_eq!(commit.author, "A <a@x> 1 +0000");
		assert_eq!(commit.message, "merge\n");
	}

	#[test]
	fn commit_matches_git_sha256() {
		// Fixture from `git commit-tree` (sha256) with fixed author/committer dates.
		let tree =
			ObjectId::from_hex("b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804")
				.unwrap();
		let commit = Commit {
			tree,
			parents: vec![],
			author: "A U Thor <author@example.com> 1700000000 +1000".to_owned(),
			committer: "C O Mitter <committer@example.com> 1700000005 -0500".to_owned(),
			signature: None,
			message: "first commit\n".to_owned(),
		};
		let id = ObjectId::compute(ObjectKind::Commit, &encode_commit(&commit));
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
		let commit = parse_commit(payload).expect("parse");
		assert_eq!(encode_commit(&commit), payload);
	}

	#[test]
	fn round_trips_a_signed_commit_and_strips_the_signature() {
		// A gpgsig header (multi-line, continuation lines start with a space), as git
		// writes it. The signature bytes here are illustrative, not a real signature.
		// Written on one line (with explicit `\n `) so the continuation spaces survive.
		let payload: &[u8] = b"tree b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804\nauthor A U Thor <author@example.com> 1700000000 +1000\ncommitter C O Mitter <committer@example.com> 1700000005 -0500\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAg\n AAAABHNoYTUxMgAAAFMAAAALc3NoLWVkMjU1MTkAAABA\n -----END SSH SIGNATURE-----\n\nsigned commit\n";
		let commit = parse_commit(payload).expect("parse");

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
		let reparsed = parse_commit(&signed).expect("reparse signed payload");
		assert_eq!(reparsed.signature, None);
		assert_eq!(reparsed.tree, commit.tree);
		assert_eq!(reparsed.message, commit.message);
	}
}
