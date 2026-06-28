use crate::text::as_str;
use crate::{ObjectError, ObjectId};

/// One entry in a tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
	/// The octal mode string (e.g. `100644`, `40000`).
	pub mode: String,
	/// The entry name.
	pub name: String,
	/// The id of the referenced object.
	pub id: ObjectId,
}

/// Parse a tree payload into its entries.
///
/// Each entry is `<mode> <name>\0<32 raw id bytes>`.
pub fn parse_tree(payload: &[u8]) -> Result<Vec<TreeEntry>, ObjectError> {
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

		if rest.len() < 32 {
			return Err(ObjectError::MalformedHeader);
		}
		let mut id = [0u8; 32];
		id.copy_from_slice(&rest[..32]);
		rest = &rest[32..];

		entries.push(TreeEntry {
			mode,
			name,
			id: ObjectId::from_bytes(id),
		});
	}

	Ok(entries)
}

/// Encode tree `entries` to the canonical git tree payload.
///
/// Entries are sorted by git's rule: by name bytes, with directory (`40000`)
/// names compared as if they ended in `/`. Each entry is `<mode> <name>\0<32
/// raw id bytes>`.
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
	let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
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

fn tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
	let mut key = entry.name.as_bytes().to_vec();
	if entry.mode == "40000" {
		key.push(b'/');
	}
	key
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ObjectKind;

	#[test]
	fn parses_a_tree() {
		let blob = ObjectId::compute(ObjectKind::Blob, b"x");
		let mut payload = b"100644 file.txt\0".to_vec();
		payload.extend_from_slice(blob.as_bytes());
		payload.extend_from_slice(b"40000 dir\0");
		let dir = ObjectId::compute(ObjectKind::Tree, b"d");
		payload.extend_from_slice(dir.as_bytes());

		let entries = parse_tree(&payload).expect("parse");
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].mode, "100644");
		assert_eq!(entries[0].name, "file.txt");
		assert_eq!(entries[0].id, blob);
		assert_eq!(entries[1].name, "dir");
		assert_eq!(entries[1].id, dir);
	}

	#[test]
	fn empty_tree_matches_git_sha256() {
		let id = ObjectId::compute(ObjectKind::Tree, &encode_tree(&[]));
		assert_eq!(
			id.to_hex(),
			"6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
		);
	}

	#[test]
	fn tree_with_one_blob_matches_git_sha256() {
		// Fixture from `git mktree` (sha256): 100644 greeting.txt -> blob("hello").
		let blob =
			ObjectId::from_hex("8aec4e4876f854f688d0ebfc8f37598f38e5fd6903cccc850ca36591175aeb60")
				.unwrap();
		let tree = encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "greeting.txt".to_owned(),
			id: blob,
		}]);
		let id = ObjectId::compute(ObjectKind::Tree, &tree);
		assert_eq!(
			id.to_hex(),
			"b5f4f26b2641070724725ca76c135b9ff2a94b3573a1cdb04223a198cfe53804"
		);
	}

	#[test]
	fn encode_round_trips_parse() {
		let blob = ObjectId::compute(ObjectKind::Blob, b"x");
		let dir = ObjectId::compute(ObjectKind::Tree, b"d");
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
		let reparsed = parse_tree(&encode_tree(&entries)).expect("parse");
		// Sorted: "dir" (as "dir/") sorts after "file.txt"? No — 'd' < 'f', so dir first.
		assert_eq!(reparsed[0].name, "dir");
		assert_eq!(reparsed[1].name, "file.txt");
	}
}
