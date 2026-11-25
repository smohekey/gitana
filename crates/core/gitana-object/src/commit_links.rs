//! A commit's tree, parents and committer time, read from its bytes.

use crate::{HashAlgorithm, ObjectError, ObjectId};

/// What links a commit into history: its tree, its parents and when it was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitLinks<H: HashAlgorithm> {
	/// The tree it records.
	pub tree: ObjectId<H>,
	/// Its parents, in order.
	pub parents: Vec<ObjectId<H>>,
	/// The committer line's time, in seconds since the epoch; `None` when the line is malformed (as
	/// [`Signature::parse`](crate::Signature::parse) judges it), which a walk needing only parents or a
	/// tree passes over and one ordered by date refuses.
	pub committer_seconds: Option<i64>,
}

/// A commit's [`CommitLinks`], read from its raw `payload` without decoding its text: its message,
/// names and other headers needn't be UTF-8 (git's `encoding` header writes them in another), as
/// [`parse_commit`](crate::parse_commit) requires. For a caller that walks history or reads a
/// commit's tree, and needs nothing of its text. The lines are read as `parse_commit` reads them; a
/// commit without a tree or committer line, or whose tree or a parent isn't an id, is malformed.
pub fn commit_links<H: HashAlgorithm>(payload: &[u8]) -> Result<CommitLinks<H>, ObjectError> {
	let header = match payload.windows(2).position(|pair| pair == b"\n\n") {
		Some(at) => &payload[..at],
		None => payload,
	};
	let id = |hex: &[u8]| -> Result<ObjectId<H>, ObjectError> {
		let hex = std::str::from_utf8(hex).map_err(|_| ObjectError::MalformedHeader)?;
		ObjectId::from_hex(hex)
	};
	let mut tree = None;
	let mut parents = Vec::new();
	let mut committer = None;
	for line in header.split(|&byte| byte == b'\n') {
		if let Some(rest) = line.strip_prefix(b"tree ") {
			tree = Some(id(rest)?);
		} else if let Some(rest) = line.strip_prefix(b"parent ") {
			parents.push(id(rest)?);
		} else if let Some(rest) = line.strip_prefix(b"committer ") {
			committer = Some(seconds(rest).ok());
		}
	}
	Ok(CommitLinks {
		tree: tree.ok_or(ObjectError::MalformedHeader)?,
		parents,
		committer_seconds: committer.ok_or(ObjectError::MalformedHeader)?,
	})
}

/// The time in an identity line's bytes (`Name <email> seconds ±hhmm`), checked as
/// [`Signature::parse`](crate::Signature::parse) checks the line: an email in its last
/// `<…>`, then the seconds and a `±hhmm` offset after it. The name is left undecoded.
fn seconds(identity: &[u8]) -> Result<i64, ObjectError> {
	let email_end = identity
		.iter()
		.rposition(|&byte| byte == b'>')
		.ok_or(ObjectError::MalformedHeader)?;
	if !identity[..email_end].contains(&b'<') {
		return Err(ObjectError::MalformedHeader);
	}
	let mut words = identity[email_end + 1..]
		.split(|byte| byte.is_ascii_whitespace())
		.filter(|word| !word.is_empty());
	let seconds = words
		.next()
		.and_then(|word| std::str::from_utf8(word).ok())
		.and_then(|word| word.parse().ok())
		.ok_or(ObjectError::MalformedHeader)?;
	let offset = words.next().ok_or(ObjectError::MalformedHeader)?;
	let well_formed = offset.len() == 5
		&& matches!(offset[0], b'+' | b'-')
		&& offset[1..].iter().all(u8::is_ascii_digit);
	if !well_formed {
		return Err(ObjectError::MalformedHeader);
	}
	Ok(seconds)
}

#[cfg(test)]
mod tests {
	use crate::Sha1;

	use super::*;

	const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

	fn links(committer: &[u8]) -> Result<CommitLinks<Sha1>, ObjectError> {
		let mut payload = format!("tree {TREE}\nparent {TREE}\nauthor A <a@e> 1 +0000\n").into_bytes();
		payload.extend_from_slice(b"committer ");
		payload.extend_from_slice(committer);
		payload.extend_from_slice(b"\nencoding ISO-8859-1\n\ncaf\xe9\n");
		commit_links::<Sha1>(&payload)
	}

	/// Read from bytes, a name or message that isn't UTF-8 is no matter; a committer line's time is
	/// read only if the line is what `Signature::parse` holds it to, its links read either way; a
	/// missing tree or committer, or a tree that isn't an id, is malformed.
	#[test]
	fn a_commit_s_links_are_read_from_its_bytes_as_its_identity_lines_are_checked() {
		let read = links(b"Andr\xe9 <a@e> 1700000000 +1300").unwrap();
		assert_eq!(read.tree.to_hex(), TREE);
		assert_eq!(read.parents.len(), 1);
		assert_eq!(read.committer_seconds, Some(1_700_000_000));
		for wrong in [
			&b"A a@e> 1700000000 +0000"[..],
			b"A <a@e>",
			b"A <a@e> soon +0000",
			b"A <a@e> 1700000000",
			b"A <a@e> 1700000000 1300",
			b"A <a@e> 1700000000 +13",
		] {
			let read = links(wrong).unwrap();
			assert_eq!(
				read.committer_seconds,
				None,
				"{}",
				String::from_utf8_lossy(wrong)
			);
			assert_eq!(read.parents.len(), 1);
		}
		assert!(commit_links::<Sha1>(b"tree nope\ncommitter A <a@e> 1 +0000\n\nm\n").is_err());
		assert!(commit_links::<Sha1>(format!("tree {TREE}\n\nmessage\n").as_bytes()).is_err());
		assert!(commit_links::<Sha1>(b"committer A <a@e> 1 +0000\n\nmessage\n").is_err());
	}

	/// A pack's enumeration reads a commit in another encoding by its links: its tree and parent.
	#[test]
	fn a_commit_in_another_encoding_is_enumerated_by_its_links() {
		let mut payload = format!("tree {TREE}\nparent {TREE}\n").into_bytes();
		payload.extend_from_slice(b"author Andr\xe9 <a@e> 1 +0000\ncommitter C <c@e> 1 +0000\n");
		payload.extend_from_slice(b"encoding ISO-8859-1\n\ncaf\xe9\n");
		let tree = ObjectId::<Sha1>::from_hex(TREE).unwrap();
		assert_eq!(
			crate::referenced_ids::<Sha1>(crate::ObjectKind::Commit, &payload).unwrap(),
			vec![tree, tree]
		);
	}
}
