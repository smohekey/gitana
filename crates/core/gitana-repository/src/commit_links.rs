//! A commit's tree, parents and committer time, read from its bytes.

use gitana_object::{HashAlgorithm, ObjectError, ObjectId};

/// What links a commit into history: its tree, its parents and when it was committed.
pub(crate) struct CommitLinks<H: HashAlgorithm> {
	pub tree: ObjectId<H>,
	pub parents: Vec<ObjectId<H>>,
	/// The committer line's time, in seconds since the epoch.
	pub committer_seconds: i64,
}

/// A commit's [`CommitLinks`], read from its raw `payload` without decoding its text: its message,
/// names and other headers needn't be UTF-8 (git's `encoding` header writes them in another), as
/// [`parse_commit`](gitana_object::parse_commit) requires. The lines are read as `parse_commit`
/// reads them; a commit without a tree or committer line is malformed.
pub(crate) fn commit_links<H: HashAlgorithm>(
	payload: &[u8],
) -> Result<CommitLinks<H>, ObjectError> {
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
			committer = Some(seconds(rest)?);
		}
	}
	Ok(CommitLinks {
		tree: tree.ok_or(ObjectError::MalformedHeader)?,
		parents,
		committer_seconds: committer.ok_or(ObjectError::MalformedHeader)?,
	})
}

/// The time in an identity line's bytes (`Name <email> seconds ±hhmm`), checked as
/// [`Signature::parse`](gitana_object::Signature::parse) checks the line: an email in its last
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
	use gitana_object::Sha1;

	use super::*;

	const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

	fn links(committer: &[u8]) -> Result<CommitLinks<Sha1>, ObjectError> {
		let mut payload = format!("tree {TREE}\nparent {TREE}\nauthor A <a@e> 1 +0000\n").into_bytes();
		payload.extend_from_slice(b"committer ");
		payload.extend_from_slice(committer);
		payload.extend_from_slice(b"\nencoding ISO-8859-1\n\ncaf\xe9\n");
		commit_links::<Sha1>(&payload)
	}

	/// Read from bytes, a name or message that isn't UTF-8 is no matter; a committer line is held to
	/// what `Signature::parse` holds it to, and a missing tree or committer is malformed.
	#[test]
	fn a_commit_s_links_are_read_from_its_bytes_as_its_identity_lines_are_checked() {
		let read = links(b"Andr\xe9 <a@e> 1700000000 +1300").unwrap();
		assert_eq!(read.tree.to_hex(), TREE);
		assert_eq!(read.parents.len(), 1);
		assert_eq!(read.committer_seconds, 1_700_000_000);
		for wrong in [
			&b"A a@e> 1700000000 +0000"[..],
			b"A <a@e>",
			b"A <a@e> soon +0000",
			b"A <a@e> 1700000000",
			b"A <a@e> 1700000000 1300",
			b"A <a@e> 1700000000 +13",
		] {
			assert!(links(wrong).is_err(), "{}", String::from_utf8_lossy(wrong));
		}
		assert!(commit_links::<Sha1>(format!("tree {TREE}\n\nmessage\n").as_bytes()).is_err());
		assert!(commit_links::<Sha1>(b"committer A <a@e> 1 +0000\n\nmessage\n").is_err());
	}
}
