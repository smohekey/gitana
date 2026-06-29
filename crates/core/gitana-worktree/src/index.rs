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

/// The unmerged index stages for a path: the common ancestor (stage 1), our side (stage 2), and
/// their side (stage 3). Any may be absent (e.g. a side that deleted the path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict<'a> {
	pub base: Option<&'a IndexEntry>,
	pub ours: Option<&'a IndexEntry>,
	pub theirs: Option<&'a IndexEntry>,
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

	/// Insert or replace the entry for its path, keeping the entries sorted. Any other stages for the
	/// path (a recorded conflict) are dropped, so staging a resolved file collapses it to stage 0.
	pub fn upsert(&mut self, entry: IndexEntry) {
		self.remove(&entry.path);
		self.insert_sorted(entry);
	}

	/// Remove every entry for `path` (all stages), if any.
	pub fn remove(&mut self, path: &str) {
		self.entries.retain(|entry| entry.path != path);
	}

	/// Record a merge conflict for `path`, replacing any existing entries with the present stages:
	/// base (stage 1), ours (stage 2), theirs (stage 3), each `(mode, oid)` or absent.
	pub fn record_conflict(
		&mut self,
		path: &str,
		base: Option<(u32, ObjectId)>,
		ours: Option<(u32, ObjectId)>,
		theirs: Option<(u32, ObjectId)>,
	) {
		self.remove(path);
		for (stage, side) in [(1u8, base), (2, ours), (3, theirs)] {
			if let Some((mode, oid)) = side {
				self.insert_sorted(IndexEntry {
					stat: Stat::default(),
					mode,
					oid,
					stage,
					assume_valid: false,
					path: path.to_owned(),
				});
			}
		}
	}

	/// The unmerged stages for `path` (base/ours/theirs), or `None` if it is not conflicted.
	pub fn conflict(&self, path: &str) -> Option<Conflict<'_>> {
		let stage = |stage: u8| {
			self
				.entries
				.iter()
				.find(|entry| entry.path == path && entry.stage == stage)
		};
		let conflict = Conflict {
			base: stage(1),
			ours: stage(2),
			theirs: stage(3),
		};
		match conflict {
			Conflict {
				base: None,
				ours: None,
				theirs: None,
			} => None,
			conflict => Some(conflict),
		}
	}

	/// Whether `path` has any conflict (stage > 0) entry.
	pub fn is_unmerged(&self, path: &str) -> bool {
		self
			.entries
			.iter()
			.any(|entry| entry.path == path && entry.stage != 0)
	}

	/// Whether the index holds any unmerged path.
	pub fn has_conflicts(&self) -> bool {
		self.entries.iter().any(|entry| entry.stage != 0)
	}

	/// The distinct paths with a conflict (stage > 0) entry, in sorted order. Entries are sorted by
	/// `(path, stage)`, so same-path stages are adjacent.
	pub fn unmerged_paths(&self) -> impl Iterator<Item = &str> {
		let mut last: Option<&str> = None;
		self
			.entries
			.iter()
			.filter(|entry| entry.stage != 0)
			.filter_map(move |entry| {
				let path = entry.path.as_str();
				if last == Some(path) {
					None
				} else {
					last = Some(path);
					Some(path)
				}
			})
	}

	/// Insert `entry` at its sorted `(path, stage)` position.
	fn insert_sorted(&mut self, entry: IndexEntry) {
		let position = self
			.entries
			.partition_point(|existing| key(existing) < key(&entry));
		self.entries.insert(position, entry);
	}

	/// Drop entries whose file/directory shape conflicts with recording `path` as a file:
	/// an ancestor recorded as a file (`path` is now under a directory), or entries recorded
	/// beneath `path` as a directory (`path` is now a file). Used when staging a type change,
	/// the way `git add` rewrites the index to match the working tree.
	pub fn remove_type_conflicts(&mut self, path: &str) {
		let mut ancestor = String::new();
		let mut components = path.split('/').peekable();
		while let Some(component) = components.next() {
			if components.peek().is_none() {
				break; // `path` itself is replaced by the caller's upsert
			}
			if !ancestor.is_empty() {
				ancestor.push('/');
			}
			ancestor.push_str(component);
			self.remove(&ancestor);
		}
		let dir_prefix = format!("{path}/");
		self
			.entries
			.retain(|entry| !entry.path.starts_with(&dir_prefix));
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

	fn oid(content: &[u8]) -> ObjectId {
		ObjectId::compute(ObjectKind::Blob, content)
	}

	#[test]
	fn record_conflict_round_trips_and_queries() {
		let mut index = Index::new();
		index.upsert(entry("clean.txt", b"x"));
		index.record_conflict(
			"f.txt",
			Some((0o100644, oid(b"base"))),
			Some((0o100644, oid(b"ours"))),
			Some((0o100644, oid(b"theirs"))),
		);

		assert!(index.has_conflicts());
		assert!(index.is_unmerged("f.txt"));
		assert!(!index.is_unmerged("clean.txt"));
		assert_eq!(index.unmerged_paths().collect::<Vec<_>>(), ["f.txt"]);

		let conflict = index.conflict("f.txt").unwrap();
		assert_eq!(conflict.base.unwrap().stage, 1);
		assert_eq!(conflict.ours.unwrap().oid, oid(b"ours"));
		assert_eq!(conflict.theirs.unwrap().oid, oid(b"theirs"));

		// All stages survive the on-disk round-trip.
		assert_eq!(Index::parse(&index.write_v4()).unwrap(), index);
	}

	#[test]
	fn upsert_and_remove_resolve_a_conflict() {
		let mut index = Index::new();
		index.record_conflict(
			"f.txt",
			Some((0o100644, oid(b"b"))),
			Some((0o100644, oid(b"o"))),
			Some((0o100644, oid(b"t"))),
		);

		// Staging the resolved file collapses every stage to a single stage-0 entry.
		index.upsert(entry("f.txt", b"resolved"));
		assert!(!index.is_unmerged("f.txt"));
		assert!(index.conflict("f.txt").is_none());
		assert_eq!(
			index.entries.iter().filter(|e| e.path == "f.txt").count(),
			1
		);
		assert_eq!(index.entry("f.txt").unwrap().stage, 0);

		// Removing drops the path entirely, even when conflicted.
		index.record_conflict("f.txt", None, Some((0o100644, oid(b"o"))), None);
		assert!(index.is_unmerged("f.txt"));
		index.remove("f.txt");
		assert!(index.entries.iter().all(|e| e.path != "f.txt"));
	}

	#[test]
	fn partial_conflict_reports_absent_stages() {
		// modify/delete: base and ours present, theirs deleted.
		let mut index = Index::new();
		index.record_conflict(
			"f.txt",
			Some((0o100644, oid(b"b"))),
			Some((0o100644, oid(b"o"))),
			None,
		);
		let conflict = index.conflict("f.txt").unwrap();
		assert!(conflict.base.is_some() && conflict.ours.is_some() && conflict.theirs.is_none());
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
