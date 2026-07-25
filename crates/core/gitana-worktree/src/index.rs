use gitana_object::{HashAlgorithm, ObjectId, decode_ewah_bounded};
use gitana_repository::{FileMode, TreeBuildEntry};

use crate::{IndexEntry, Stat, WorktreeError};

/// The split-index `link` extension: the shared index id, plus the **raw** EWAH delete/replace bitmap bytes
/// (delete then replace, concatenated) that transform the shared (base) index into the effective one. Used
/// only by `load_index` (which can load the shared file), via [`merge_split_index`].
///
/// The bitmaps are kept **undecoded** here on purpose: their header `bit_size` is attacker-controlled and a
/// tiny payload can inflate into a ~512 MiB decode, so they are decoded only in [`merge_split_index`], where
/// the shared index's entry count gives a real bound to reject a crafted header (see `decode_ewah_bounded`).
pub(crate) struct SplitIndexLink<H: HashAlgorithm> {
	/// The shared index id — `<git-dir>/sharedindex.<hex>`. All-zero means "no shared index" (a base itself).
	pub shared_oid: ObjectId<H>,
	/// The delete bitmap followed by the replace bitmap, still EWAH-encoded (decoded, bounded, at merge time).
	pub bitmaps_raw: Vec<u8>,
}

/// Whether `oid` is the all-zero id (the split-index sentinel for "no shared index file").
pub(crate) fn is_null_oid<H: HashAlgorithm>(oid: &ObjectId<H>) -> bool {
	oid.as_bytes().iter().all(|&b| b == 0)
}

/// Parse a split-index `link` extension payload: the shared index id followed by the delete then replace
/// EWAH bitmaps. The bitmaps are retained **raw** (not decoded) — decoding is deferred to
/// [`merge_split_index`], which knows the shared index's entry count and so can reject a crafted `bit_size`
/// before allocating (see [`SplitIndexLink`]).
fn parse_link_extension<H: HashAlgorithm>(
	payload: &[u8],
) -> Result<SplitIndexLink<H>, WorktreeError> {
	let oid_len = H::RAW_LEN;
	let raw = payload
		.get(..oid_len)
		.ok_or_else(|| WorktreeError::Malformed("link extension: short oid".to_owned()))?;
	let shared_oid =
		ObjectId::<H>::from_bytes(raw).map_err(|_| WorktreeError::Malformed("link oid".to_owned()))?;
	Ok(SplitIndexLink {
		shared_oid,
		bitmaps_raw: payload[oid_len..].to_vec(),
	})
}

/// Merge a shared (`base`) index and a split index using the `link` bitmaps, producing the effective index
/// (git's split-index model). The split index's **first `popcount(replace)` entries** replace the shared
/// entries at the replace bitmap's set positions (a replacement with an empty name keeps the shared name);
/// the shared entries at the delete bitmap's set positions are removed; and the split index's remaining
/// entries are additions. The result is re-sorted by `(path, stage)`.
pub(crate) fn merge_split_index<H: HashAlgorithm>(
	base: Index<H>,
	split: Index<H>,
	link: &SplitIndexLink<H>,
) -> Result<Index<H>, WorktreeError> {
	let mut base_entries = base.entries;
	// Decode the delete/replace bitmaps now, bounded by the shared index's entry count: a valid position
	// addresses an entry in `base`, so `bit_size` (highest set bit + 1) cannot exceed `base_entries.len()`.
	// This rejects a crafted `link` extension whose tiny payload claims a `u32`-scale `bit_size` before it can
	// force a ~512 MiB decode or a billions-of-positions `set_bits` walk.
	let max_bits = base_entries.len() as u64;
	let (delete_bits, consumed) = decode_ewah_bounded(&link.bitmaps_raw, max_bits)
		.map_err(|_| WorktreeError::Malformed("link delete bitmap".to_owned()))?;
	let (replace_bits, _) = decode_ewah_bounded(&link.bitmaps_raw[consumed..], max_bits)
		.map_err(|_| WorktreeError::Malformed("link replace bitmap".to_owned()))?;
	let replace_positions: Vec<u32> = replace_bits.set_bits().collect();
	if split.entries.len() < replace_positions.len() {
		return Err(WorktreeError::Malformed(
			"split index: fewer entries than replacements".to_owned(),
		));
	}
	// Replacements: the k-th replace bit (ascending) is filled by the k-th split entry.
	for (k, &pos) in replace_positions.iter().enumerate() {
		let base_entry = base_entries
			.get(pos as usize)
			.ok_or_else(|| WorktreeError::Malformed("split index: replace out of range".to_owned()))?;
		let name = if split.entries[k].path.is_empty() {
			base_entry.path.clone()
		} else {
			split.entries[k].path.clone()
		};
		base_entries[pos as usize] = IndexEntry {
			path: name,
			..split.entries[k].clone()
		};
	}
	// Deletions: mark shared positions for removal (positions are unchanged by in-place replacement).
	let mut deleted = vec![false; base_entries.len()];
	for pos in delete_bits.set_bits() {
		*deleted
			.get_mut(pos as usize)
			.ok_or_else(|| WorktreeError::Malformed("split index: delete out of range".to_owned()))? = true;
	}
	let mut entries: Vec<IndexEntry<H>> = base_entries
		.into_iter()
		.zip(deleted)
		.filter_map(|(entry, dead)| (!dead).then_some(entry))
		.collect();
	// Additions: the split index's entries after the replacements.
	entries.extend(split.entries.into_iter().skip(replace_positions.len()));
	entries.sort_by(|a, b| key(a).cmp(&key(b)));
	Ok(Index { entries })
}

const SIGNATURE: &[u8; 4] = b"DIRC";

/// The tree file mode for a raw index mode (git stores one of three blob modes).
fn file_mode(mode: u32) -> FileMode {
	match mode {
		0o100755 => FileMode::Executable,
		0o120000 => FileMode::Symlink,
		_ => FileMode::Regular,
	}
}

/// The git index (`.git/index`, the "DIRC" file): the staging area.
///
/// Reads versions 2–4 and writes version 4 (prefix-compressed paths). Object ids and
/// the trailing checksum are sized by the hash algorithm `H`. Entries are kept sorted
/// by `(path, stage)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index<H: HashAlgorithm> {
	/// Staged entries, sorted by `(path, stage)`.
	pub entries: Vec<IndexEntry<H>>,
}

/// The unmerged index stages for a path: the common ancestor (stage 1), our side (stage 2), and
/// their side (stage 3). Any may be absent (e.g. a side that deleted the path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict<'a, H: HashAlgorithm> {
	pub base: Option<&'a IndexEntry<H>>,
	pub ours: Option<&'a IndexEntry<H>>,
	pub theirs: Option<&'a IndexEntry<H>>,
}

impl<H: HashAlgorithm> Default for Index<H> {
	fn default() -> Self {
		Index {
			entries: Vec::new(),
		}
	}
}

impl<H: HashAlgorithm> Index<H> {
	/// An empty index.
	pub fn new() -> Self {
		Self::default()
	}

	/// The stage-0 entries as tree-build entries — the content a commit captures. Conflicted
	/// (stage > 0) entries have no stage-0 slot and are skipped, so resolve them first.
	pub fn tree_entries(&self) -> Vec<TreeBuildEntry<H>> {
		self
			.entries
			.iter()
			.filter(|entry| entry.stage == 0)
			.map(|entry| TreeBuildEntry {
				path: entry.path.clone(),
				mode: file_mode(entry.mode),
				id: entry.oid,
			})
			.collect()
	}

	/// The stage-0 entry for `path`, if present.
	pub fn entry(&self, path: &str) -> Option<&IndexEntry<H>> {
		self
			.entries
			.iter()
			.find(|entry| entry.path == path && entry.stage == 0)
	}

	/// Whether `path` has a stage-0 entry excluded from the working tree (`skip_worktree` set) — a
	/// sparse-checkout path. Such entries are invisible to working-tree pathspec operations
	/// (`add`/`restore`): their absent file is neither restaged as a modification nor recorded as a
	/// deletion, matching git's sparse-checkout pathspec exclusion.
	pub fn is_sparse(&self, path: &str) -> bool {
		self.entry(path).is_some_and(|entry| entry.skip_worktree)
	}

	/// Insert or replace the entry for its path, keeping the entries sorted. Any other stages for the
	/// path (a recorded conflict) are dropped, so staging a resolved file collapses it to stage 0.
	pub fn upsert(&mut self, entry: IndexEntry<H>) {
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
		base: Option<(u32, ObjectId<H>)>,
		ours: Option<(u32, ObjectId<H>)>,
		theirs: Option<(u32, ObjectId<H>)>,
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
					skip_worktree: false,
					path: path.to_owned(),
				});
			}
		}
	}

	/// The unmerged stages for `path` (base/ours/theirs), or `None` if it is not conflicted.
	pub fn conflict(&self, path: &str) -> Option<Conflict<'_, H>> {
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
	fn insert_sorted(&mut self, entry: IndexEntry<H>) {
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
		Ok(Self::parse_with_link(bytes)?.0)
	}

	/// Parse index bytes, additionally returning the **split-index** `link` extension when present (its shared
	/// index id and the delete/replace EWAH bitmaps). A caller with filesystem access (`load_index`) uses it to
	/// load and merge the shared index; a plain `parse` ignores it.
	pub(crate) fn parse_with_link(
		bytes: &[u8],
	) -> Result<(Self, Option<SplitIndexLink<H>>), WorktreeError> {
		let checksum_len = H::RAW_LEN;
		if bytes.len() < 12 + checksum_len || &bytes[0..4] != SIGNATURE {
			return Err(WorktreeError::Malformed("bad signature".to_owned()));
		}
		let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
		if !(2..=4).contains(&version) {
			return Err(WorktreeError::Malformed(format!("version {version}")));
		}
		let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;

		let body_end = bytes.len() - checksum_len;
		if H::digest(&[&bytes[..body_end]]).as_ref() != &bytes[body_end..] {
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

			let oid = read_oid::<H>(bytes, &mut cursor)?;
			let flags = read_u16(bytes, &mut cursor)?;
			let assume_valid = flags & 0x8000 != 0;
			let stage = ((flags >> 12) & 0x3) as u8;
			let mut skip_worktree = false;
			if flags & 0x4000 != 0 {
				if version < 3 {
					return Err(WorktreeError::Malformed("extended flag in v2".to_owned()));
				}
				// Extended flags (v3+): bit 0x4000 = skip-worktree (sparse), 0x2000 = intent-to-add.
				let extended = read_u16(bytes, &mut cursor)?;
				skip_worktree = extended & 0x4000 != 0;
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
				skip_worktree,
				path,
			});
		}

		// Extensions follow the entries, up to the trailing checksum: each is a 4-byte signature, a 4-byte
		// big-endian length, then the payload. We only interpret the split-index `link` extension; all others
		// (cache-tree, resolve-undo, untracked-cache, fsmonitor, …) are skipped — they are hints git can
		// recompute, so ignoring them yields a correct (if uncached) index.
		let mut link = None;
		while cursor + 8 <= body_end {
			let sig: [u8; 4] = bytes[cursor..cursor + 4].try_into().unwrap();
			let size = u32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
			let payload_start = cursor + 8;
			let payload_end = payload_start
				.checked_add(size)
				.filter(|&e| e <= body_end)
				.ok_or_else(|| WorktreeError::Malformed("index extension size".to_owned()))?;
			if &sig == b"link" {
				link = Some(parse_link_extension::<H>(
					&bytes[payload_start..payload_end],
				)?);
			}
			cursor = payload_end;
		}

		Ok((Index { entries }, link))
	}

	/// Serialise to index version 4 (prefix-compressed paths) with an `H` trailer.
	pub fn write_v4(&self) -> Vec<u8> {
		let mut sorted: Vec<&IndexEntry<H>> = self.entries.iter().collect();
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
			// A skip-worktree entry needs the extended-flag bit set and a following extended-flags word, so the
			// sparse marker round-trips (git would otherwise re-check the omitted path against the working tree).
			if entry.skip_worktree {
				flags |= 0x4000;
			}
			out.extend_from_slice(&flags.to_be_bytes());
			if entry.skip_worktree {
				out.extend_from_slice(&0x4000u16.to_be_bytes()); // extended flags: skip-worktree
			}

			let path = entry.path.as_bytes();
			let common = common_prefix(prev, path);
			out.extend_from_slice(&encode_varint((prev.len() - common) as u64));
			out.extend_from_slice(&path[common..]);
			out.push(0);
			prev = path;
		}

		let checksum = H::digest(&[&out]);
		out.extend_from_slice(checksum.as_ref());
		out
	}
}

fn key<H: HashAlgorithm>(entry: &IndexEntry<H>) -> (&[u8], u8) {
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

fn read_oid<H: HashAlgorithm>(
	bytes: &[u8],
	cursor: &mut usize,
) -> Result<ObjectId<H>, WorktreeError> {
	let end = *cursor + H::RAW_LEN;
	let slice = bytes
		.get(*cursor..end)
		.ok_or_else(|| WorktreeError::Malformed("truncated oid".to_owned()))?;
	*cursor = end;
	ObjectId::from_bytes(slice).map_err(|_| WorktreeError::Malformed("bad oid".to_owned()))
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
	use gitana_object::{ObjectKind, Sha256};

	use super::*;

	fn entry(path: &str, content: &[u8]) -> IndexEntry<Sha256> {
		IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid: ObjectId::<Sha256>::compute(ObjectKind::Blob, content),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			path: path.to_owned(),
		}
	}

	#[test]
	fn v4_round_trips() {
		let mut index = Index::<Sha256>::new();
		index.upsert(entry("src/lib.rs", b"a"));
		index.upsert(entry("src/main.rs", b"b"));
		index.upsert(entry("README.md", b"c"));

		let parsed = Index::parse(&index.write_v4()).expect("parse");
		assert_eq!(parsed, index);
		// Sorted by path.
		let paths: Vec<&str> = parsed.entries.iter().map(|e| e.path.as_str()).collect();
		assert_eq!(paths, ["README.md", "src/lib.rs", "src/main.rs"]);
	}

	fn oid(content: &[u8]) -> ObjectId<Sha256> {
		ObjectId::<Sha256>::compute(ObjectKind::Blob, content)
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
		let mut bytes = Index::<Sha256>::new().write_v4();
		let last = bytes.len() - 1;
		bytes[last] ^= 0xff;
		assert!(matches!(
			Index::<Sha256>::parse(&bytes),
			Err(WorktreeError::ChecksumMismatch)
		));
	}
}
