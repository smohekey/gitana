use sha2::{Digest, Sha256};

use gitana_object::ObjectId;

use crate::{IndexEntry, Stat, WorktreeError};

const SIGNATURE: &[u8; 4] = b"DIRC";
const CHECKSUM_LEN: usize = 32;
const OID_LEN: usize = 32;

/// The git index (`.git/index`, the "DIRC" file): the staging area.
///
/// Reads versions 2–4 and writes version 4 (prefix-compressed paths), in sha256.
/// Entries are kept sorted by `(path, stage)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
	/// Staged entries, sorted by `(path, stage)`.
	pub entries: Vec<IndexEntry>,
}

impl Index {
	/// An empty index.
	pub fn new() -> Self {
		Self::default()
	}

	/// The stage-0 entry for `path`, if present.
	pub fn entry(&self, path: &str) -> Option<&IndexEntry> {
		self
			.entries
			.iter()
			.find(|entry| entry.path == path && entry.stage == 0)
	}

	/// Insert or replace an entry, keeping the entries sorted.
	pub fn upsert(&mut self, entry: IndexEntry) {
		self
			.entries
			.retain(|existing| !(existing.path == entry.path && existing.stage == entry.stage));
		let position = self
			.entries
			.partition_point(|existing| key(existing) < key(&entry));
		self.entries.insert(position, entry);
	}

	/// Remove the stage-0 entry for `path`, if any.
	pub fn remove(&mut self, path: &str) {
		self
			.entries
			.retain(|entry| !(entry.path == path && entry.stage == 0));
	}

	/// Parse index bytes (DIRC v2–v4), verifying the trailing checksum.
	pub fn parse(bytes: &[u8]) -> Result<Self, WorktreeError> {
		if bytes.len() < 12 + CHECKSUM_LEN || &bytes[0..4] != SIGNATURE {
			return Err(WorktreeError::Malformed("bad signature".to_owned()));
		}
		let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
		if !(2..=4).contains(&version) {
			return Err(WorktreeError::Malformed(format!("version {version}")));
		}
		let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;

		let body_end = bytes.len() - CHECKSUM_LEN;
		if Sha256::digest(&bytes[..body_end]).as_slice() != &bytes[body_end..] {
			return Err(WorktreeError::ChecksumMismatch);
		}

		let mut cursor = 12;
		let mut entries = Vec::with_capacity(count);
		let mut prev: Vec<u8> = Vec::new();
		for _ in 0..count {
			let entry_start = cursor;
			let stat = Stat {
				ctime_sec: read_u32(bytes, &mut cursor)?,
				ctime_nsec: read_u32(bytes, &mut cursor)?,
				mtime_sec: read_u32(bytes, &mut cursor)?,
				mtime_nsec: read_u32(bytes, &mut cursor)?,
				dev: read_u32(bytes, &mut cursor)?,
				ino: read_u32(bytes, &mut cursor)?,
				..Stat::default()
			};
			let mode = read_u32(bytes, &mut cursor)?;
			let uid = read_u32(bytes, &mut cursor)?;
			let gid = read_u32(bytes, &mut cursor)?;
			let size = read_u32(bytes, &mut cursor)?;
			let stat = Stat {
				uid,
				gid,
				size,
				..stat
			};

			let oid = read_oid(bytes, &mut cursor)?;
			let flags = read_u16(bytes, &mut cursor)?;
			let assume_valid = flags & 0x8000 != 0;
			let stage = ((flags >> 12) & 0x3) as u8;
			if flags & 0x4000 != 0 {
				if version < 3 {
					return Err(WorktreeError::Malformed("extended flag in v2".to_owned()));
				}
				read_u16(bytes, &mut cursor)?; // extended flags (skip-worktree, intent-to-add)
			}

			let path_bytes = if version == 4 {
				let strip = decode_varint(bytes, &mut cursor)?;
				let suffix = read_until_nul(bytes, &mut cursor)?;
				let keep = prev
					.len()
					.checked_sub(strip)
					.ok_or_else(|| WorktreeError::Malformed("v4 strip underflow".to_owned()))?;
				let mut path = prev[..keep].to_vec();
				path.extend_from_slice(suffix);
				path
			} else {
				let name = read_until_nul(bytes, &mut cursor)?.to_vec();
				// v2/v3 pad the entry (incl. the NUL) to a multiple of 8 bytes.
				let unpadded = cursor - entry_start; // already past the NUL
				let pad = (8 - (unpadded % 8)) % 8;
				cursor += pad;
				name
			};

			let path = String::from_utf8(path_bytes.clone())
				.map_err(|_| WorktreeError::Malformed("non-UTF-8 path".to_owned()))?;
			prev = path_bytes;
			entries.push(IndexEntry {
				stat,
				mode,
				oid,
				stage,
				assume_valid,
				path,
			});
		}

		Ok(Index { entries })
	}

	/// Serialise to index version 4 (prefix-compressed paths) with a sha256 trailer.
	pub fn write_v4(&self) -> Vec<u8> {
		let mut sorted: Vec<&IndexEntry> = self.entries.iter().collect();
		sorted.sort_by(|a, b| key(a).cmp(&key(b)));

		let mut out = Vec::new();
		out.extend_from_slice(SIGNATURE);
		out.extend_from_slice(&4u32.to_be_bytes());
		out.extend_from_slice(&(sorted.len() as u32).to_be_bytes());

		let mut prev: &[u8] = &[];
		for entry in sorted {
			for field in [
				entry.stat.ctime_sec,
				entry.stat.ctime_nsec,
				entry.stat.mtime_sec,
				entry.stat.mtime_nsec,
				entry.stat.dev,
				entry.stat.ino,
				entry.mode,
				entry.stat.uid,
				entry.stat.gid,
				entry.stat.size,
			] {
				out.extend_from_slice(&field.to_be_bytes());
			}
			out.extend_from_slice(entry.oid.as_bytes());

			let name_len = entry.path.len().min(0xFFF) as u16;
			let mut flags = name_len | ((entry.stage as u16) << 12);
			if entry.assume_valid {
				flags |= 0x8000;
			}
			out.extend_from_slice(&flags.to_be_bytes());

			let path = entry.path.as_bytes();
			let common = common_prefix(prev, path);
			out.extend_from_slice(&encode_varint((prev.len() - common) as u64));
			out.extend_from_slice(&path[common..]);
			out.push(0);
			prev = path;
		}

		let checksum = Sha256::digest(&out);
		out.extend_from_slice(&checksum);
		out
	}
}

fn key(entry: &IndexEntry) -> (&[u8], u8) {
	(entry.path.as_bytes(), entry.stage)
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
	a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, WorktreeError> {
	let end = *cursor + 4;
	let slice = bytes
		.get(*cursor..end)
		.ok_or_else(|| WorktreeError::Malformed("truncated u32".to_owned()))?;
	*cursor = end;
	Ok(u32::from_be_bytes(slice.try_into().unwrap()))
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, WorktreeError> {
	let end = *cursor + 2;
	let slice = bytes
		.get(*cursor..end)
		.ok_or_else(|| WorktreeError::Malformed("truncated u16".to_owned()))?;
	*cursor = end;
	Ok(u16::from_be_bytes(slice.try_into().unwrap()))
}

fn read_oid(bytes: &[u8], cursor: &mut usize) -> Result<ObjectId, WorktreeError> {
	let end = *cursor + OID_LEN;
	let slice = bytes
		.get(*cursor..end)
		.ok_or_else(|| WorktreeError::Malformed("truncated oid".to_owned()))?;
	*cursor = end;
	let mut id = [0u8; 32];
	id.copy_from_slice(slice);
	Ok(ObjectId::from_bytes(id))
}

fn read_until_nul<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], WorktreeError> {
	let nul = bytes[*cursor..]
		.iter()
		.position(|&b| b == 0)
		.map(|i| *cursor + i)
		.ok_or_else(|| WorktreeError::Malformed("unterminated name".to_owned()))?;
	let name = &bytes[*cursor..nul];
	*cursor = nul + 1;
	Ok(name)
}

/// git's index-v4 path varint (the offset-style base-128 encoding).
fn decode_varint(bytes: &[u8], cursor: &mut usize) -> Result<usize, WorktreeError> {
	let mut byte = *bytes
		.get(*cursor)
		.ok_or_else(|| WorktreeError::Malformed("truncated varint".to_owned()))?;
	*cursor += 1;
	let mut value = (byte & 0x7f) as usize;
	while byte & 0x80 != 0 {
		value += 1;
		byte = *bytes
			.get(*cursor)
			.ok_or_else(|| WorktreeError::Malformed("truncated varint".to_owned()))?;
		*cursor += 1;
		value = (value << 7) + (byte & 0x7f) as usize;
	}
	Ok(value)
}

fn encode_varint(mut value: u64) -> Vec<u8> {
	let mut buf = vec![(value & 0x7f) as u8];
	value >>= 7;
	while value != 0 {
		value -= 1;
		buf.push(0x80 | (value & 0x7f) as u8);
		value >>= 7;
	}
	buf.reverse();
	buf
}

#[cfg(test)]
mod tests {
	use gitana_object::ObjectKind;

	use super::*;

	fn entry(path: &str, content: &[u8]) -> IndexEntry {
		IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid: ObjectId::compute(ObjectKind::Blob, content),
			stage: 0,
			assume_valid: false,
			path: path.to_owned(),
		}
	}

	#[test]
	fn v4_round_trips() {
		let mut index = Index::new();
		index.upsert(entry("src/lib.rs", b"a"));
		index.upsert(entry("src/main.rs", b"b"));
		index.upsert(entry("README.md", b"c"));

		let parsed = Index::parse(&index.write_v4()).expect("parse");
		assert_eq!(parsed, index);
		// Sorted by path.
		let paths: Vec<&str> = parsed.entries.iter().map(|e| e.path.as_str()).collect();
		assert_eq!(paths, ["README.md", "src/lib.rs", "src/main.rs"]);
	}

	#[test]
	fn rejects_bad_checksum() {
		let mut bytes = Index::new().write_v4();
		let last = bytes.len() - 1;
		bytes[last] ^= 0xff;
		assert!(matches!(
			Index::parse(&bytes),
			Err(WorktreeError::ChecksumMismatch)
		));
	}
}
