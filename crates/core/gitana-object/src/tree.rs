use crate::text::as_str;
use crate::{HashAlgorithm, ObjectError, ObjectId};

/// One entry in a tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry<H: HashAlgorithm> {
	/// The octal mode string (e.g. `100644`, `40000`).
	pub mode: String,
	/// The entry name.
	pub name: String,
	/// The id of the referenced object.
	pub id: ObjectId<H>,
}

/// Parse a tree payload into its entries.
///
/// Each entry is `<mode> <name>\0<raw id bytes>`, where the id width is the hash
/// algorithm's [`HashAlgorithm::RAW_LEN`].
pub fn parse_tree<H: HashAlgorithm>(payload: &[u8]) -> Result<Vec<TreeEntry<H>>, ObjectError> {
	let mut entries = Vec::new();
	let mut rest = payload;

	while !rest.is_empty() {
		let space = rest
			.iter()
			.position(|&b| b == b' ')
			.ok_or(ObjectError::MalformedHeader)?;
		let mode = as_str(&rest[..space])?.to_owned();
		rest = &rest[space + 1..];

		let nul = rest
			.iter()
			.position(|&b| b == 0)
			.ok_or(ObjectError::MalformedHeader)?;
		let name = as_str(&rest[..nul])?.to_owned();
		rest = &rest[nul + 1..];

		if rest.len() < H::RAW_LEN {
			return Err(ObjectError::MalformedHeader);
		}
		let id = ObjectId::from_bytes(&rest[..H::RAW_LEN])?;
		rest = &rest[H::RAW_LEN..];

		entries.push(TreeEntry { mode, name, id });
	}

	Ok(entries)
}

/// Encode tree `entries` to the canonical git tree payload.
///
/// Entries are sorted by git's rule: by name bytes, with directory (`40000`)
/// names compared as if they ended in `/`. Each entry is `<mode> <name>\0<raw
/// id bytes>`.
pub fn encode_tree<H: HashAlgorithm>(entries: &[TreeEntry<H>]) -> Vec<u8> {
	let mut sorted: Vec<&TreeEntry<H>> = entries.iter().collect();
	sorted.sort_by_cached_key(|entry| tree_sort_key(entry));

	let mut out = Vec::new();
	for entry in sorted {
		out.extend_from_slice(entry.mode.as_bytes());
		out.push(b' ');
		out.extend_from_slice(entry.name.as_bytes());
		out.push(0);
		out.extend_from_slice(entry.id.as_bytes());
	}
	out
}

fn tree_sort_key<H: HashAlgorithm>(entry: &TreeEntry<H>) -> Vec<u8> {
	let mut key = entry.name.as_bytes().to_vec();
	if entry.mode == "40000" {
		key.push(b'/');
	}
	key
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ObjectKind, Sha1, Sha256};

	#[test]
	fn parses_a_tree() {
		let blob = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x");
		let mut payload = b"100644 file.txt\0".to_vec();
		payload.extend_from_slice(blob.as_bytes());
		payload.extend_from_slice(b"40000 dir\0");
		let dir = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"d");
		payload.extend_from_slice(dir.as_bytes());

		let entries = parse_tree::<Sha256>(&payload).expect("parse");
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].mode, "100644");
		assert_eq!(entries[0].name, "file.txt");
		assert_eq!(entries[0].id, blob);
		assert_eq!(entries[1].name, "dir");
		assert_eq!(entries[1].id, dir);
	}

	#[test]
	fn empty_tree_matches_git_sha256() {
		let id = ObjectId::<Sha256>::compute(ObjectKind::Tree, &encode_tree::<Sha256>(&[]));
		assert_eq!(
			id.to_hex(),
			"6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
		);
	}

	#[test]
	fn tree_with_one_blob_matches_git_sha256() {
		// Fixture from `git mktree` (sha256): 100644 greeting.txt -> blob("hello").
		let blob = ObjectId::<Sha256>::from_hex(
			"8aec4e4876f854f688d0ebfc8f37598f38e5fd6903cccc850ca36591175aeb60",
		)
		.unwrap();
		let tree = encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "greeting.txt".to_owned(),
			id: blob,
		}]);
		let id = ObjectId::<Sha256>::compute(ObjectKind::Tree, &tree);
		assert_eq!(
			id.to_hex(),
			"b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804"
		);
	}

	#[test]
	fn sha1_tree_round_trips() {
		// A SHA-1 tree must use 20-byte ids end to end.
		let blob = ObjectId::<Sha1>::compute(ObjectKind::Blob, b"x");
		let dir = ObjectId::<Sha1>::compute(ObjectKind::Tree, b"d");
		let entries = vec![
			TreeEntry {
				mode: "40000".to_owned(),
				name: "dir".to_owned(),
				id: dir,
			},
			TreeEntry {
				mode: "100644".to_owned(),
				name: "file.txt".to_owned(),
				id: blob,
			},
		];
		let encoded = encode_tree(&entries);
		let reparsed = parse_tree::<Sha1>(&encoded).expect("parse");
		assert_eq!(reparsed[0].name, "dir");
		assert_eq!(reparsed[0].id, dir);
		assert_eq!(reparsed[1].name, "file.txt");
		assert_eq!(reparsed[1].id, blob);
	}

	#[test]
	fn encode_round_trips_parse() {
		let blob = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x");
		let dir = ObjectId::<Sha256>::compute(ObjectKind::Tree, b"d");
		let entries = vec![
			TreeEntry {
				mode: "40000".to_owned(),
				name: "dir".to_owned(),
				id: dir,
			},
			TreeEntry {
				mode: "100644".to_owned(),
				name: "file.txt".to_owned(),
				id: blob,
			},
		];
		let reparsed = parse_tree::<Sha256>(&encode_tree(&entries)).expect("parse");
		// Sorted: "dir" (as "dir/") sorts after "file.txt"? No — 'd' < 'f', so dir first.
		assert_eq!(reparsed[0].name, "dir");
		assert_eq!(reparsed[1].name, "file.txt");
	}
}
