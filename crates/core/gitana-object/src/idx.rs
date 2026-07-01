//! Reader/writer for git pack index files (`.idx`, version 2).
//!
//! A `.idx` maps each object id in a packfile to its byte offset in the `.pack` (and the
//! CRC-32 of its packed bytes), so an object can be located by id without decoding the
//! whole pack. Generic over the hash algorithm `H`: object ids and the two trailing
//! checksums are `H::RAW_LEN` bytes wide. Build one for a pack with
//! [`crate::pack_index_entries`] + [`encode_pack_index`]; parse one with
//! [`decode_pack_index`].

use crate::{HashAlgorithm, ObjectError, ObjectId};

/// The v2 signature, `\377tOc` — a value the (positive) v1 object count can never begin with.
const MAGIC: [u8; 4] = [0xff, 0x74, 0x4f, 0x63];
const VERSION: u32 = 2;
/// The fanout table: 256 big-endian u32s.
const FANOUT_LEN: usize = 256 * 4;
/// An offset-table entry with this bit set indexes the 64-bit large-offset table instead.
const LARGE_OFFSET_FLAG: u32 = 0x8000_0000;

/// One object's placement in a pack: its id, byte offset within the `.pack`, and the
/// CRC-32 of its packed bytes (the entry header plus compressed data). The unit a `.idx`
/// records per object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackIndexEntry<H: HashAlgorithm> {
	/// The object's id.
	pub id: ObjectId<H>,
	/// The object's byte offset within the packfile.
	pub offset: u64,
	/// The CRC-32 of the object's packed bytes (git's `--strict` integrity check).
	pub crc32: u32,
}

/// Encode a version-2 pack index (`.idx`) for `entries`, describing the pack whose trailer
/// hash is `pack_checksum` (`H::RAW_LEN` bytes). Entries are written sorted by id (git's
/// order), so their order in the slice does not matter.
///
/// Fails with [`ObjectError::MalformedPackIndex`] if two entries share an id: a `.idx` maps
/// each id to exactly one offset, so a duplicate (e.g. from a malformed pack storing an
/// object twice) has no valid encoding — and [`decode_pack_index`] would reject the result.
pub fn encode_pack_index<H: HashAlgorithm>(
	entries: &[PackIndexEntry<H>],
	pack_checksum: &[u8],
) -> Result<Vec<u8>, ObjectError> {
	// The pack checksum occupies a fixed H::RAW_LEN field; a wrong length would shift the
	// trailing index checksum and yield an index this decoder and git reject.
	if pack_checksum.len() != H::RAW_LEN {
		return Err(ObjectError::MalformedPackIndex);
	}
	let mut sorted: Vec<&PackIndexEntry<H>> = entries.iter().collect();
	sorted.sort_by(|a, b| a.id.cmp(&b.id));
	if sorted.windows(2).any(|pair| pair[0].id == pair[1].id) {
		return Err(ObjectError::MalformedPackIndex);
	}

	let mut out = Vec::new();
	out.extend_from_slice(&MAGIC);
	out.extend_from_slice(&VERSION.to_be_bytes());

	// Fanout: fanout[b] = number of ids whose first byte is <= b. Count per first byte, then
	// prefix-sum, so the last entry is the total object count.
	let mut fanout = [0u32; 256];
	for entry in &sorted {
		fanout[entry.id.as_bytes()[0] as usize] += 1;
	}
	let mut cumulative = 0u32;
	for bucket in &mut fanout {
		cumulative += *bucket;
		*bucket = cumulative;
	}
	for bucket in fanout {
		out.extend_from_slice(&bucket.to_be_bytes());
	}

	for entry in &sorted {
		out.extend_from_slice(entry.id.as_bytes());
	}
	for entry in &sorted {
		out.extend_from_slice(&entry.crc32.to_be_bytes());
	}
	// Offsets: a value below 2^31 is the offset; otherwise the low 31 bits index the
	// 64-bit large-offset table, appended after the small offsets in first-seen order.
	let mut large_offsets = Vec::new();
	for entry in &sorted {
		if entry.offset < u64::from(LARGE_OFFSET_FLAG) {
			out.extend_from_slice(&(entry.offset as u32).to_be_bytes());
		} else {
			let index = large_offsets.len() as u32;
			out.extend_from_slice(&(LARGE_OFFSET_FLAG | index).to_be_bytes());
			large_offsets.push(entry.offset);
		}
	}
	for offset in large_offsets {
		out.extend_from_slice(&offset.to_be_bytes());
	}

	out.extend_from_slice(pack_checksum);
	let checksum = H::digest(&[&out]);
	out.extend_from_slice(checksum.as_ref());
	Ok(out)
}

/// A parsed version-2 pack index (`.idx`): its entries sorted by id, plus the trailer hash
/// of the pack it describes. Look objects up with [`Self::lookup`] / [`Self::offset_of`].
pub struct PackIndex<H: HashAlgorithm> {
	entries: Vec<PackIndexEntry<H>>,
	pack_checksum: Vec<u8>,
}

impl<H: HashAlgorithm> PackIndex<H> {
	/// The number of objects the index covers.
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	/// Whether the index covers no objects.
	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// The entries, sorted by id.
	pub fn entries(&self) -> &[PackIndexEntry<H>] {
		&self.entries
	}

	/// The trailer hash of the pack this index describes (to check against a `.pack`).
	pub fn pack_checksum(&self) -> &[u8] {
		&self.pack_checksum
	}

	/// Find an object's entry by id, via binary search over the sorted ids.
	pub fn lookup(&self, id: &ObjectId<H>) -> Option<&PackIndexEntry<H>> {
		self
			.entries
			.binary_search_by(|entry| entry.id.cmp(id))
			.ok()
			.map(|i| &self.entries[i])
	}

	/// The object's byte offset within the pack, if the id is present.
	pub fn offset_of(&self, id: &ObjectId<H>) -> Option<u64> {
		self.lookup(id).map(|entry| entry.offset)
	}
}

/// Parse a version-2 pack index (`.idx`), verifying its magic, version, and trailing
/// checksum. The entries come out sorted by id.
pub fn decode_pack_index<H: HashAlgorithm>(bytes: &[u8]) -> Result<PackIndex<H>, ObjectError> {
	let raw = H::RAW_LEN;
	let header = 8 + FANOUT_LEN;
	// magic + version + fanout, then at least the pack + index checksums.
	if bytes.len() < header + 2 * raw {
		return Err(ObjectError::MalformedPackIndex);
	}
	if bytes[0..4] != MAGIC || read_u32(&bytes[4..8]) != VERSION {
		return Err(ObjectError::MalformedPackIndex);
	}
	let body_end = bytes.len() - raw;
	if H::digest(&[&bytes[..body_end]]).as_ref() != &bytes[body_end..] {
		return Err(ObjectError::MalformedPackIndex);
	}

	// Object count = the last fanout entry.
	let n = read_u32(&bytes[header - 4..header]) as usize;
	let ids_start = header;
	let crc_start = ids_start + n * raw;
	let off_start = crc_start + n * 4;
	let large_start = off_start + n * 4;
	// The id, crc, and small-offset tables must fit before the pack checksum.
	if large_start > body_end - raw {
		return Err(ObjectError::MalformedPackIndex);
	}

	// Small offsets: a value with the flag bit set indexes the large-offset table.
	let mut small = Vec::with_capacity(n);
	let mut large_count = 0usize;
	for i in 0..n {
		let value = read_u32(&bytes[off_start + i * 4..off_start + (i + 1) * 4]);
		if value & LARGE_OFFSET_FLAG != 0 {
			large_count += 1;
		}
		small.push(value);
	}
	// The large-offset table fills exactly the gap between the small offsets and the pack
	// checksum — one 64-bit entry per flagged small offset.
	if large_start + large_count * 8 != body_end - raw {
		return Err(ObjectError::MalformedPackIndex);
	}

	let mut entries: Vec<PackIndexEntry<H>> = Vec::with_capacity(n);
	for (i, value) in small.into_iter().enumerate() {
		let id = ObjectId::from_bytes(&bytes[ids_start + i * raw..ids_start + (i + 1) * raw])?;
		// Ids are strictly ascending in a well-formed index; `lookup` binary-searches on that.
		// Reject an out-of-order (or duplicate) name table even if its checksum was recomputed.
		if entries.last().is_some_and(|last| id <= last.id) {
			return Err(ObjectError::MalformedPackIndex);
		}
		let crc32 = read_u32(&bytes[crc_start + i * 4..crc_start + (i + 1) * 4]);
		let offset = if value & LARGE_OFFSET_FLAG == 0 {
			u64::from(value)
		} else {
			let index = (value & !LARGE_OFFSET_FLAG) as usize;
			if index >= large_count {
				return Err(ObjectError::MalformedPackIndex);
			}
			read_u64(&bytes[large_start + index * 8..large_start + (index + 1) * 8])
		};
		entries.push(PackIndexEntry { id, offset, crc32 });
	}

	// The fanout must match the ids: fanout[b] is the number of ids whose first byte is <= b.
	// Recompute it from the (sorted) ids and compare bucket for bucket, so a corrupt table
	// with a recomputed checksum is still rejected (git treats such an index as damaged). This
	// also enforces monotonicity, since a prefix-sum is non-decreasing.
	let mut cumulative = 0u32;
	let mut first_byte = 0usize;
	for bucket in 0..256 {
		while first_byte < entries.len() && (entries[first_byte].id.as_bytes()[0] as usize) == bucket {
			cumulative += 1;
			first_byte += 1;
		}
		if read_u32(&bytes[8 + bucket * 4..8 + bucket * 4 + 4]) != cumulative {
			return Err(ObjectError::MalformedPackIndex);
		}
	}

	Ok(PackIndex {
		entries,
		pack_checksum: bytes[body_end - raw..body_end].to_vec(),
	})
}

fn read_u32(bytes: &[u8]) -> u32 {
	u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64(bytes: &[u8]) -> u64 {
	u64::from_be_bytes([
		bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
	])
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ObjectKind, Sha1, Sha256};

	fn entry<H: HashAlgorithm>(payload: &[u8], offset: u64, crc32: u32) -> PackIndexEntry<H> {
		PackIndexEntry {
			id: ObjectId::<H>::compute(ObjectKind::Blob, payload),
			offset,
			crc32,
		}
	}

	#[test]
	fn round_trips_and_looks_up() {
		let entries = vec![
			entry::<Sha256>(b"one", 12, 0x1111_1111),
			entry::<Sha256>(b"two", 40, 0x2222_2222),
			entry::<Sha256>(b"three", 77, 0x3333_3333),
		];
		let pack_checksum = [0x5au8; 32];
		let idx = encode_pack_index(&entries, &pack_checksum).expect("encode");

		let parsed = decode_pack_index::<Sha256>(&idx).expect("decode");
		assert_eq!(parsed.len(), 3);
		assert_eq!(parsed.pack_checksum(), &pack_checksum);
		// Entries come back sorted by id, so this is not necessarily input order.
		for original in &entries {
			let found = parsed.lookup(&original.id).expect("present");
			assert_eq!(found, original);
			assert_eq!(parsed.offset_of(&original.id), Some(original.offset));
		}
		// A stranger id is absent.
		let stranger = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"absent");
		assert!(parsed.lookup(&stranger).is_none());
	}

	#[test]
	fn round_trips_large_offsets() {
		// A pack index does not exercise 64-bit offsets until a pack exceeds 2 GiB, so drive
		// the large-offset table directly with a synthetic offset above 2^31.
		let entries = vec![
			entry::<Sha256>(b"small", 100, 1),
			entry::<Sha256>(b"huge", 0x1_2345_6789, 2),
		];
		let idx = encode_pack_index(&entries, &[0u8; 32]).expect("encode");
		let parsed = decode_pack_index::<Sha256>(&idx).expect("decode");
		for original in &entries {
			assert_eq!(parsed.offset_of(&original.id), Some(original.offset));
		}
	}

	#[test]
	fn round_trips_under_sha1() {
		let entries = vec![entry::<Sha1>(b"a", 12, 7), entry::<Sha1>(b"b", 34, 8)];
		let idx = encode_pack_index(&entries, &[0xabu8; 20]).expect("encode");
		let parsed = decode_pack_index::<Sha1>(&idx).expect("decode");
		assert_eq!(parsed.len(), 2);
		assert_eq!(parsed.pack_checksum(), &[0xabu8; 20]);
		for original in &entries {
			assert_eq!(parsed.lookup(&original.id), Some(original));
		}
	}

	#[test]
	fn rejects_a_corrupt_index() {
		let idx = encode_pack_index(&[entry::<Sha256>(b"x", 12, 1)], &[0u8; 32]).expect("encode");
		// Flip a byte in the fanout: the trailing checksum no longer matches.
		let mut corrupt = idx.clone();
		corrupt[10] ^= 0xff;
		assert!(matches!(
			decode_pack_index::<Sha256>(&corrupt),
			Err(ObjectError::MalformedPackIndex)
		));
		// A bad magic is rejected too.
		let mut bad_magic = idx;
		bad_magic[0] ^= 0xff;
		assert!(matches!(
			decode_pack_index::<Sha256>(&bad_magic),
			Err(ObjectError::MalformedPackIndex)
		));
	}

	#[test]
	fn rejects_an_unsorted_index() {
		// Swap the two ids in the name table so they are out of order, then repair the trailing
		// checksum — only the ordering is wrong. Decode must still reject it, because `lookup`
		// relies on the sorted-ids invariant.
		let mut idx = encode_pack_index(
			&[
				entry::<Sha256>(b"alpha", 12, 1),
				entry::<Sha256>(b"beta", 40, 2),
			],
			&[0u8; 32],
		)
		.expect("encode");
		let raw = 32;
		let ids = 8 + 256 * 4; // after magic + version + fanout
		for i in 0..raw {
			idx.swap(ids + i, ids + raw + i);
		}
		let body_end = idx.len() - raw;
		let checksum = <Sha256 as HashAlgorithm>::digest(&[&idx[..body_end]]);
		idx[body_end..].copy_from_slice(checksum.as_ref());

		assert!(matches!(
			decode_pack_index::<Sha256>(&idx),
			Err(ObjectError::MalformedPackIndex)
		));
	}

	#[test]
	fn rejects_a_corrupt_fanout() {
		// Bump an interior fanout bucket, then repair the trailing checksum: only the fanout is
		// inconsistent with the id table. Decode must reject it (git treats it as corrupt).
		let mut idx = encode_pack_index(
			&[
				entry::<Sha256>(b"one", 12, 1),
				entry::<Sha256>(b"two", 40, 2),
			],
			&[0u8; 32],
		)
		.expect("encode");
		// Fanout bucket 0 sits at offset 8, right after magic + version.
		let bumped = read_u32(&idx[8..12]).wrapping_add(1);
		idx[8..12].copy_from_slice(&bumped.to_be_bytes());
		let body_end = idx.len() - 32;
		let checksum = <Sha256 as HashAlgorithm>::digest(&[&idx[..body_end]]);
		idx[body_end..].copy_from_slice(checksum.as_ref());

		assert!(matches!(
			decode_pack_index::<Sha256>(&idx),
			Err(ObjectError::MalformedPackIndex)
		));
	}

	#[test]
	fn rejects_wrong_length_pack_checksum() {
		// The pack checksum must be H::RAW_LEN (32 for SHA-256); a short one has no canonical
		// placement, so encoding fails rather than emit a misaligned index.
		let entries = [entry::<Sha256>(b"x", 12, 1)];
		assert!(matches!(
			encode_pack_index(&entries, &[0u8; 20]),
			Err(ObjectError::MalformedPackIndex)
		));
	}

	#[test]
	fn rejects_duplicate_ids() {
		// The same id twice has no valid encoding: a .idx maps each id to one offset, and our
		// own decoder rejects a duplicated name table. `b"same"` hashes identically both times.
		let first = entry::<Sha256>(b"same", 12, 1);
		let second = entry::<Sha256>(b"same", 40, 2);
		assert!(matches!(
			encode_pack_index(&[first, second], &[0u8; 32]),
			Err(ObjectError::MalformedPackIndex)
		));
	}
}
