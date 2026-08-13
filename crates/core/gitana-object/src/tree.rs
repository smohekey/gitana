use std::collections::BTreeSet;

use crate::text::as_str;
use crate::{HashAlgorithm, ObjectError, ObjectId};

const STANDARD_MODES: [&[u8]; 5] = [b"100644", b"100755", b"120000", b"40000", b"160000"];

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

/// Validate a raw tree payload without requiring entry names to be UTF-8.
///
/// Valid trees use canonical modes and git's directory-aware byte ordering.
/// Entry names must be non-empty single path components other than `.` or `..`,
/// names cannot be repeated, and referenced object ids cannot be all zeroes.
pub fn validate_tree_structure<H: HashAlgorithm>(payload: &[u8]) -> Result<(), ObjectError> {
	let mut names = BTreeSet::new();
	let mut previous_key: Option<Vec<u8>> = None;
	let mut rest = payload;

	while !rest.is_empty() {
		let space = rest
			.iter()
			.position(|&byte| byte == b' ')
			.ok_or(ObjectError::MalformedHeader)?;
		let mode = &rest[..space];
		if !STANDARD_MODES.contains(&mode) {
			return Err(ObjectError::InvalidTreeMode);
		}
		rest = &rest[space + 1..];

		let nul = rest
			.iter()
			.position(|&byte| byte == 0)
			.ok_or(ObjectError::MalformedHeader)?;
		let name = &rest[..nul];
		if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
			return Err(ObjectError::InvalidTreeName);
		}
		if !names.insert(name.to_vec()) {
			return Err(ObjectError::DuplicateTreeEntry);
		}
		rest = &rest[nul + 1..];

		if rest.len() < H::RAW_LEN {
			return Err(ObjectError::MalformedHeader);
		}
		let raw_id = &rest[..H::RAW_LEN];
		if raw_id.iter().all(|byte| *byte == 0) {
			return Err(ObjectError::NullTreeEntry);
		}
		rest = &rest[H::RAW_LEN..];

		let mut key = name.to_vec();
		if mode == b"40000" {
			key.push(b'/');
		}
		if previous_key
			.as_ref()
			.is_some_and(|previous| previous >= &key)
		{
			return Err(ObjectError::TreeNotSorted);
		}
		previous_key = Some(key);
	}

	Ok(())
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

	#[test]
	fn validates_non_utf8_names_as_raw_bytes() {
		let blob = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x");
		let mut payload = b"100644 before\0".to_vec();
		payload.extend_from_slice(blob.as_bytes());
		payload.extend_from_slice(b"100644 \xffafter\0");
		payload.extend_from_slice(blob.as_bytes());

		validate_tree_structure::<Sha256>(&payload).expect("valid raw tree");
		assert!(parse_tree::<Sha256>(&payload).is_err());
	}

	#[test]
	fn rejects_invalid_tree_names() {
		for name in [b"".as_slice(), b".", b"..", b"a/b"] {
			let payload = raw_entry::<Sha256>(b"100644", name, &[1; 32]);
			assert!(matches!(
				validate_tree_structure::<Sha256>(&payload),
				Err(ObjectError::InvalidTreeName)
			));
		}
	}

	#[test]
	fn rejects_non_canonical_tree_modes_and_null_ids() {
		let invalid_mode = raw_entry::<Sha1>(b"040000", b"dir", &[1; 20]);
		assert!(matches!(
			validate_tree_structure::<Sha1>(&invalid_mode),
			Err(ObjectError::InvalidTreeMode)
		));

		let null_id = raw_entry::<Sha1>(b"100644", b"file", &[0; 20]);
		assert!(matches!(
			validate_tree_structure::<Sha1>(&null_id),
			Err(ObjectError::NullTreeEntry)
		));
	}

	#[test]
	fn rejects_duplicate_tree_names() {
		let mut payload = raw_entry::<Sha256>(b"100644", b"same", &[1; 32]);
		payload.extend_from_slice(&raw_entry::<Sha256>(b"100755", b"same", &[2; 32]));

		assert!(matches!(
			validate_tree_structure::<Sha256>(&payload),
			Err(ObjectError::DuplicateTreeEntry)
		));
	}

	#[test]
	fn rejects_non_canonical_directory_aware_order() {
		let mut payload = raw_entry::<Sha256>(b"40000", b"foo", &[1; 32]);
		payload.extend_from_slice(&raw_entry::<Sha256>(b"100644", b"foo.bar", &[2; 32]));

		assert!(matches!(
			validate_tree_structure::<Sha256>(&payload),
			Err(ObjectError::TreeNotSorted)
		));
	}

	fn raw_entry<H: HashAlgorithm>(mode: &[u8], name: &[u8], id: &[u8]) -> Vec<u8> {
		assert_eq!(id.len(), H::RAW_LEN);
		let mut payload = Vec::new();
		payload.extend_from_slice(mode);
		payload.push(b' ');
		payload.extend_from_slice(name);
		payload.push(0);
		payload.extend_from_slice(id);
		payload
	}
}
