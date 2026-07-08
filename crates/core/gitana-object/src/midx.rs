//! Reader/writer for git's multi-pack-index (the `multi-pack-index` file, version 1).
//!
//! A MIDX indexes the objects of *several* packs at once: one binary search over all of them
//! yields the pack an object lives in and its byte offset there, replacing a scan of every pack's
//! `.idx`. Generic over the hash algorithm `H` (object ids and the trailing checksum are
//! `H::RAW_LEN` wide; the header records a hash version of 1 for SHA-1, 2 for SHA-256). Build one
//! with [`encode_multi_pack_index`]; parse one with [`decode_multi_pack_index`]; look objects up
//! with [`MultiPackIndex::lookup`].

use std::sync::OnceLock;

use crate::{HashAlgorithm, ObjectError, ObjectId};

const MAGIC: [u8; 4] = *b"MIDX";
const VERSION: u8 = 1;
/// Chunk ids (4-byte tags), written in this order.
const CHUNK_PNAM: [u8; 4] = *b"PNAM";
const CHUNK_OIDF: [u8; 4] = *b"OIDF";
const CHUNK_OIDL: [u8; 4] = *b"OIDL";
const CHUNK_OOFF: [u8; 4] = *b"OOFF";
const CHUNK_LOFF: [u8; 4] = *b"LOFF";
/// The reverse index: the bitmap object order (see [`crate::pack_order`]). Present only in a
/// MIDX written to carry reachability bitmaps.
const CHUNK_RIDX: [u8; 4] = *b"RIDX";
const FANOUT_LEN: usize = 256 * 4;
/// The 12-byte header: magic, version, hash version, chunk count, base count, pack count.
const HEADER_LEN: usize = 12;
/// One chunk-lookup-table row: a 4-byte id and an 8-byte file offset.
const TABLE_ROW_LEN: usize = 12;
/// An `OOFF` offset with this bit set indexes the 64-bit large-offset (`LOFF`) chunk instead.
const LARGE_OFFSET_FLAG: u32 = 0x8000_0000;

/// The hash-version byte a MIDX records for `H` (git: 1 = SHA-1, 2 = SHA-256).
fn hash_version<H: HashAlgorithm>() -> u8 {
	match H::RAW_LEN {
		20 => 1,
		32 => 2,
		_ => 0,
	}
}

/// One object's placement across the indexed packs: its id, the pack it lives in (an index into
/// the MIDX's sorted pack-name list), and its byte offset within that pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidxEntry<H: HashAlgorithm> {
	/// The object's id.
	pub id: ObjectId<H>,
	/// The pack index (into the encoder's sorted `pack_names`).
	pub pack_id: u32,
	/// The object's byte offset within that pack.
	pub offset: u64,
}

/// A parsed multi-pack-index: the pack names it covers (sorted), and each object's id → its pack
/// (an index into `pack_names`) and byte offset.
pub struct MultiPackIndex<H: HashAlgorithm> {
	pack_names: Vec<String>,
	ids: Vec<ObjectId<H>>,
	locations: Vec<(u32, u64)>,
	/// The `RIDX` reverse index (bitmap object order), present only in a bitmap-carrying MIDX:
	/// `reverse_index[bitmap_position]` is the lexical index into [`Self::ids`].
	reverse_index: Option<Vec<u32>>,
	/// The inverse of [`Self::reverse_index`]: `forward_index[lexical] = bitmap_position`. Built lazily
	/// on the first [`Self::bitmap_position`] query and cached; `None` when there is no reverse index.
	forward_index: OnceLock<Option<Vec<u32>>>,
	/// The MIDX's trailing checksum — a reachability `.bitmap` names the MIDX it belongs to by this.
	checksum: Vec<u8>,
}

impl<H: HashAlgorithm> MultiPackIndex<H> {
	/// The pack basenames the index covers, sorted (a `pack_id` indexes into this).
	pub fn pack_names(&self) -> &[String] {
		&self.pack_names
	}

	/// The number of objects the index covers.
	pub fn len(&self) -> usize {
		self.ids.len()
	}

	/// Whether the index covers no objects.
	pub fn is_empty(&self) -> bool {
		self.ids.is_empty()
	}

	/// Find `id`, returning `(pack index into [`Self::pack_names`], byte offset in that pack)`.
	pub fn lookup(&self, id: &ObjectId<H>) -> Option<(usize, u64)> {
		self
			.ids
			.binary_search(id)
			.ok()
			.map(|i| (self.locations[i].0 as usize, self.locations[i].1))
	}

	/// The ids in lexical (id-sorted) order — the order a reachability bitmap's positions index
	/// through [`Self::reverse_index`].
	pub fn object_ids(&self) -> &[ObjectId<H>] {
		&self.ids
	}

	/// The `RIDX` reverse index if this MIDX carries one: `reverse_index()[bitmap_position]` is the
	/// lexical index into [`Self::object_ids`]. `None` for a MIDX written without bitmaps.
	pub fn reverse_index(&self) -> Option<&[u32]> {
		self.reverse_index.as_deref()
	}

	/// The object at a reachability-bitmap position, via the reverse index. `None` if this MIDX has
	/// no reverse index or the position is out of range.
	pub fn object_at_bitmap_position(&self, position: usize) -> Option<&ObjectId<H>> {
		let lexical = *self.reverse_index()?.get(position)?;
		self.ids.get(lexical as usize)
	}

	/// The lexical (id-sorted) index of `id`, or `None` if absent. This is the position a MIDX
	/// reachability bitmap records for a bitmapped commit (git's "nth object"), distinct from the
	/// bitmap object order used by [`Self::object_at_bitmap_position`].
	pub fn object_position(&self, id: &ObjectId<H>) -> Option<usize> {
		self.ids.binary_search(id).ok()
	}

	/// The reachability-bitmap position of `id` — the inverse of [`Self::object_at_bitmap_position`].
	/// `None` if this MIDX has no reverse index (only bitmap-carrying MIDXs do) or `id` is absent. The
	/// inverse table is built once on first call and cached, so a bitmap consumer can test a target's
	/// bit directly (`reachability.get(position)`) rather than materializing a commit's whole closure.
	pub fn bitmap_position(&self, id: &ObjectId<H>) -> Option<u32> {
		let forward = self.forward_index.get_or_init(|| {
			self.reverse_index.as_ref().map(|reverse| {
				let mut forward = vec![0u32; reverse.len()];
				for (bitmap_position, &lexical) in reverse.iter().enumerate() {
					forward[lexical as usize] = bitmap_position as u32;
				}
				forward
			})
		});
		let lexical = self.object_position(id)?;
		forward.as_ref()?.get(lexical).copied()
	}

	/// The MIDX's trailing checksum — a reachability `.bitmap` binds to its MIDX by this value.
	pub fn checksum(&self) -> &[u8] {
		&self.checksum
	}
}

/// Encode a version-1 multi-pack-index over `pack_names` (their basenames, e.g. `pack-<hex>.pack`)
/// and `entries` (one per object, referencing a pack by its index into `pack_names`).
///
/// `pack_names` must be strictly ascending (git's ASCII sort, so also unique). `entries` are
/// sorted by id and deduplicated (an id present in several packs keeps its lowest `pack_id`);
/// every `pack_id` must be a valid index into `pack_names`. Fails with
/// [`ObjectError::MalformedMultiPackIndex`] otherwise.
pub fn encode_multi_pack_index<H: HashAlgorithm>(
	pack_names: &[String],
	entries: &[MidxEntry<H>],
) -> Result<Vec<u8>, ObjectError> {
	encode_midx(pack_names, entries, None)
}

/// Like [`encode_multi_pack_index`], but also emit the `RIDX` reverse-index chunk (the bitmap
/// object order, see [`crate::pack_order`]) with `preferred_pack` leading the order — as git does
/// when writing a MIDX that carries reachability bitmaps. `preferred_pack` must index `pack_names`.
pub fn encode_multi_pack_index_with_reverse_index<H: HashAlgorithm>(
	pack_names: &[String],
	entries: &[MidxEntry<H>],
	preferred_pack: u32,
) -> Result<Vec<u8>, ObjectError> {
	encode_midx(pack_names, entries, Some(preferred_pack))
}

fn encode_midx<H: HashAlgorithm>(
	pack_names: &[String],
	entries: &[MidxEntry<H>],
	preferred_pack: Option<u32>,
) -> Result<Vec<u8>, ObjectError> {
	if pack_names.is_empty() {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	if pack_names.windows(2).any(|w| w[0] >= w[1]) {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let pack_count = pack_names.len();
	if preferred_pack.is_some_and(|p| p as usize >= pack_count) {
		return Err(ObjectError::MalformedMultiPackIndex);
	}

	// Sort by id, then drop duplicate ids keeping one copy. Git's rule: the preferred pack's copy
	// wins when present (so a bitmap position lands on the preferred pack), else the lowest pack_id.
	let mut sorted: Vec<&MidxEntry<H>> = entries.iter().collect();
	sorted.sort_by(|a, b| {
		a.id.cmp(&b.id).then_with(|| {
			let a_preferred = preferred_pack == Some(a.pack_id);
			let b_preferred = preferred_pack == Some(b.pack_id);
			b_preferred
				.cmp(&a_preferred)
				.then(a.pack_id.cmp(&b.pack_id))
		})
	});
	sorted.dedup_by(|a, b| a.id == b.id);
	if sorted.iter().any(|e| e.pack_id as usize >= pack_count) {
		return Err(ObjectError::MalformedMultiPackIndex);
	}

	// PNAM: NUL-terminated names, padded with NULs to a 4-byte boundary (as git writes it).
	let mut pnam = Vec::new();
	for name in pack_names {
		pnam.extend_from_slice(name.as_bytes());
		pnam.push(0);
	}
	while pnam.len() % 4 != 0 {
		pnam.push(0);
	}

	// OIDF: cumulative counts of ids by first byte.
	let mut fanout = [0u32; 256];
	for entry in &sorted {
		fanout[entry.id.as_bytes()[0] as usize] += 1;
	}
	let mut cumulative = 0u32;
	for bucket in &mut fanout {
		cumulative += *bucket;
		*bucket = cumulative;
	}
	let mut oidf = Vec::with_capacity(FANOUT_LEN);
	for bucket in fanout {
		oidf.extend_from_slice(&bucket.to_be_bytes());
	}

	// OIDL: the sorted ids. OOFF: pack id + offset. Following git, the 64-bit LOFF chunk exists
	// only when some offset does not fit in 32 bits (≥ 2^32); then every offset that would set the
	// high bit (≥ 2^31) is spilled to LOFF so the inline value's high bit unambiguously flags a
	// LOFF index. Without LOFF, a value in [2^31, 2^32) is stored inline in full.
	let large_needed = sorted
		.iter()
		.any(|entry| entry.offset > u64::from(u32::MAX));
	let mut oidl = Vec::with_capacity(sorted.len() * H::RAW_LEN);
	let mut ooff = Vec::with_capacity(sorted.len() * 8);
	let mut loff = Vec::new();
	for entry in &sorted {
		oidl.extend_from_slice(entry.id.as_bytes());
		ooff.extend_from_slice(&entry.pack_id.to_be_bytes());
		if large_needed && entry.offset >= u64::from(LARGE_OFFSET_FLAG) {
			let index = (loff.len() / 8) as u32;
			ooff.extend_from_slice(&(LARGE_OFFSET_FLAG | index).to_be_bytes());
			loff.extend_from_slice(&entry.offset.to_be_bytes());
		} else {
			ooff.extend_from_slice(&(entry.offset as u32).to_be_bytes());
		}
	}

	let mut chunks: Vec<([u8; 4], Vec<u8>)> = vec![
		(CHUNK_PNAM, pnam),
		(CHUNK_OIDF, oidf),
		(CHUNK_OIDL, oidl),
		(CHUNK_OOFF, ooff),
	];
	if !loff.is_empty() {
		chunks.push((CHUNK_LOFF, loff));
	}
	// RIDX: the bitmap object order, appended last as git does. `order[i]` is the lexical index of
	// the object at bitmap position `i`; the chunk is that table as big-endian u32s.
	if let Some(preferred) = preferred_pack {
		let locations: Vec<(u32, u64)> = sorted.iter().map(|e| (e.pack_id, e.offset)).collect();
		let mut ridx = Vec::with_capacity(locations.len() * 4);
		for lexical in crate::pack_order(&locations, preferred) {
			ridx.extend_from_slice(&lexical.to_be_bytes());
		}
		chunks.push((CHUNK_RIDX, ridx));
	}

	let mut out = Vec::new();
	out.extend_from_slice(&MAGIC);
	out.push(VERSION);
	out.push(hash_version::<H>());
	out.push(chunks.len() as u8);
	out.push(0); // base MIDX count
	out.extend_from_slice(&(pack_count as u32).to_be_bytes());

	// Chunk lookup table: one row per chunk plus a terminating (id 0, end-offset) row.
	let mut offset = (HEADER_LEN + (chunks.len() + 1) * TABLE_ROW_LEN) as u64;
	for (id, data) in &chunks {
		out.extend_from_slice(id);
		out.extend_from_slice(&offset.to_be_bytes());
		offset += data.len() as u64;
	}
	out.extend_from_slice(&[0u8; 4]);
	out.extend_from_slice(&offset.to_be_bytes());

	for (_, data) in &chunks {
		out.extend_from_slice(data);
	}

	let checksum = H::digest(&[&out]);
	out.extend_from_slice(checksum.as_ref());
	Ok(out)
}

/// Parse a version-1 multi-pack-index, verifying its magic, version, hash version, chunk table,
/// trailing checksum, sorted-id and fanout invariants, and pack references.
pub fn decode_multi_pack_index<H: HashAlgorithm>(
	bytes: &[u8],
) -> Result<MultiPackIndex<H>, ObjectError> {
	let raw = H::RAW_LEN;
	if bytes.len() < HEADER_LEN + raw {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	if bytes[0..4] != MAGIC || bytes[4] != VERSION || bytes[5] != hash_version::<H>() || bytes[7] != 0
	{
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let chunk_count = bytes[6] as usize;
	let pack_count = read_u32(&bytes[8..12]) as usize;

	let body_end = bytes.len() - raw;
	if H::digest(&[&bytes[..body_end]]).as_ref() != &bytes[body_end..] {
		return Err(ObjectError::MalformedMultiPackIndex);
	}

	// Chunk lookup table: `chunk_count + 1` rows of (id, offset), offsets non-decreasing, the
	// first at the end of the table and the last at the body end.
	let table_end = HEADER_LEN + (chunk_count + 1) * TABLE_ROW_LEN;
	if table_end > body_end {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let mut rows: Vec<([u8; 4], usize)> = Vec::with_capacity(chunk_count + 1);
	for i in 0..=chunk_count {
		let o = HEADER_LEN + i * TABLE_ROW_LEN;
		let id = [bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]];
		let off = read_u64(&bytes[o + 4..o + 12]) as usize;
		rows.push((id, off));
	}
	if rows[0].1 != table_end || rows[chunk_count].1 != body_end {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	if rows.windows(2).any(|w| w[0].1 > w[1].1) {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let find = |want: &[u8; 4]| -> Option<(usize, usize)> {
		(0..chunk_count)
			.find(|&i| &rows[i].0 == want)
			.map(|i| (rows[i].1, rows[i + 1].1))
	};

	let (pnam_s, pnam_e) = find(&CHUNK_PNAM).ok_or(ObjectError::MalformedMultiPackIndex)?;
	let (oidf_s, oidf_e) = find(&CHUNK_OIDF).ok_or(ObjectError::MalformedMultiPackIndex)?;
	let (oidl_s, oidl_e) = find(&CHUNK_OIDL).ok_or(ObjectError::MalformedMultiPackIndex)?;
	let (ooff_s, ooff_e) = find(&CHUNK_OOFF).ok_or(ObjectError::MalformedMultiPackIndex)?;
	let loff = find(&CHUNK_LOFF);
	let ridx = find(&CHUNK_RIDX);

	// PNAM: split on NUL, skipping padding; must yield exactly `pack_count` ascending names.
	let mut pack_names = Vec::with_capacity(pack_count);
	for part in bytes[pnam_s..pnam_e].split(|&b| b == 0) {
		if part.is_empty() {
			continue;
		}
		let name = std::str::from_utf8(part).map_err(|_| ObjectError::MalformedMultiPackIndex)?;
		pack_names.push(name.to_owned());
	}
	if pack_names.len() != pack_count || pack_names.windows(2).any(|w| w[0] >= w[1]) {
		return Err(ObjectError::MalformedMultiPackIndex);
	}

	if oidf_e - oidf_s != FANOUT_LEN {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let n = read_u32(&bytes[oidf_s + 255 * 4..oidf_s + 256 * 4]) as usize;
	if oidl_e - oidl_s != n * raw || ooff_e - ooff_s != n * 8 {
		return Err(ObjectError::MalformedMultiPackIndex);
	}
	let loff_bytes = loff.map(|(s, e)| &bytes[s..e]);

	let mut ids: Vec<ObjectId<H>> = Vec::with_capacity(n);
	let mut locations: Vec<(u32, u64)> = Vec::with_capacity(n);
	for i in 0..n {
		let id = ObjectId::from_bytes(&bytes[oidl_s + i * raw..oidl_s + (i + 1) * raw])?;
		if ids.last().is_some_and(|last| &id <= last) {
			return Err(ObjectError::MalformedMultiPackIndex);
		}
		let base = ooff_s + i * 8;
		let pack_id = read_u32(&bytes[base..base + 4]);
		if pack_id as usize >= pack_count {
			return Err(ObjectError::MalformedMultiPackIndex);
		}
		let raw_off = read_u32(&bytes[base + 4..base + 8]);
		let offset = match loff_bytes {
			// With a LOFF chunk the high bit flags a 64-bit large offset stored there; otherwise
			// (git omits LOFF when every offset is < 2^32) the 32-bit value is the offset in full.
			Some(table) if raw_off & LARGE_OFFSET_FLAG != 0 => {
				let index = (raw_off & !LARGE_OFFSET_FLAG) as usize;
				if (index + 1) * 8 > table.len() {
					return Err(ObjectError::MalformedMultiPackIndex);
				}
				read_u64(&table[index * 8..index * 8 + 8])
			}
			_ => u64::from(raw_off),
		};
		ids.push(id);
		locations.push((pack_id, offset));
	}

	// Fanout must match the ids (as for `.idx`): reject a corrupt table even if its checksum was
	// recomputed. This also confirms monotonicity.
	let mut cumulative = 0u32;
	let mut next = 0usize;
	for bucket in 0..256 {
		while next < ids.len() && (ids[next].as_bytes()[0] as usize) == bucket {
			cumulative += 1;
			next += 1;
		}
		if read_u32(&bytes[oidf_s + bucket * 4..oidf_s + bucket * 4 + 4]) != cumulative {
			return Err(ObjectError::MalformedMultiPackIndex);
		}
	}

	// RIDX (optional): a table of `n` big-endian u32 lexical indices — the bitmap object order. Each
	// entry must be a valid, unique index into the ids (a permutation of `0..n`).
	let reverse_index = match ridx {
		Some((s, e)) => {
			if e - s != n * 4 {
				return Err(ObjectError::MalformedMultiPackIndex);
			}
			let mut order = Vec::with_capacity(n);
			let mut seen = vec![false; n];
			for i in 0..n {
				let lexical = read_u32(&bytes[s + i * 4..s + i * 4 + 4]);
				let slot = seen
					.get_mut(lexical as usize)
					.ok_or(ObjectError::MalformedMultiPackIndex)?;
				if *slot {
					return Err(ObjectError::MalformedMultiPackIndex);
				}
				*slot = true;
				order.push(lexical);
			}
			Some(order)
		}
		None => None,
	};

	Ok(MultiPackIndex {
		pack_names,
		ids,
		locations,
		reverse_index,
		forward_index: OnceLock::new(),
		checksum: bytes[body_end..].to_vec(),
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

	fn entry<H: HashAlgorithm>(payload: &[u8], pack_id: u32, offset: u64) -> MidxEntry<H> {
		MidxEntry {
			id: ObjectId::<H>::compute(ObjectKind::Blob, payload),
			pack_id,
			offset,
		}
	}

	#[test]
	fn round_trips_and_looks_up_across_packs() {
		let names = vec!["pack-a.pack".to_owned(), "pack-b.pack".to_owned()];
		let entries = vec![
			entry::<Sha256>(b"one", 0, 12),
			entry::<Sha256>(b"two", 1, 40),
			entry::<Sha256>(b"three", 0, 77),
			entry::<Sha256>(b"four", 1, 5),
		];
		let bytes = encode_multi_pack_index(&names, &entries).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");

		assert_eq!(midx.len(), 4);
		assert_eq!(midx.pack_names(), names.as_slice());
		for e in &entries {
			let (pack, offset) = midx.lookup(&e.id).expect("present");
			assert_eq!(pack, e.pack_id as usize);
			assert_eq!(offset, e.offset);
			assert_eq!(midx.pack_names()[pack], names[e.pack_id as usize]);
		}
		let stranger = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"absent");
		assert!(midx.lookup(&stranger).is_none());
	}

	#[test]
	fn round_trips_the_reverse_index() {
		let names = vec!["pack-a.pack".to_owned(), "pack-b.pack".to_owned()];
		let entries = vec![
			entry::<Sha256>(b"one", 0, 12),
			entry::<Sha256>(b"two", 1, 40),
			entry::<Sha256>(b"three", 0, 77),
			entry::<Sha256>(b"four", 1, 5),
		];

		// Without a preferred pack there is no reverse index.
		let plain =
			decode_multi_pack_index::<Sha256>(&encode_multi_pack_index(&names, &entries).unwrap())
				.expect("decode plain");
		assert!(plain.reverse_index().is_none());

		// With one, the RIDX round-trips and equals `pack_order` over the lexical (id-sorted) order.
		let bytes = encode_multi_pack_index_with_reverse_index(&names, &entries, 1).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		let locations: Vec<(u32, u64)> = midx
			.object_ids()
			.iter()
			.map(|id| {
				let (pack, offset) = midx.lookup(id).unwrap();
				(pack as u32, offset)
			})
			.collect();
		assert_eq!(
			midx.reverse_index(),
			Some(crate::pack_order(&locations, 1).as_slice())
		);

		// Bitmap position 0 is the preferred pack's lowest-offset object ("four", pack 1 @ 5).
		let four = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"four");
		assert_eq!(midx.object_at_bitmap_position(0), Some(&four));
		assert!(midx.object_at_bitmap_position(midx.len()).is_none());

		// A preferred pack outside the pack list is rejected.
		assert!(encode_multi_pack_index_with_reverse_index(&names, &entries, 2).is_err());
	}

	#[test]
	fn bitmap_position_inverts_object_at_bitmap_position() {
		let names = vec!["pack-a.pack".to_owned(), "pack-b.pack".to_owned()];
		let entries = vec![
			entry::<Sha256>(b"one", 0, 12),
			entry::<Sha256>(b"two", 1, 40),
			entry::<Sha256>(b"three", 0, 77),
			entry::<Sha256>(b"four", 1, 5),
		];

		// A MIDX with a reverse index: `bitmap_position` is the exact inverse of
		// `object_at_bitmap_position` across every position.
		let bytes = encode_multi_pack_index_with_reverse_index(&names, &entries, 1).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		for position in 0..midx.len() {
			let id = midx.object_at_bitmap_position(position).expect("in range");
			assert_eq!(midx.bitmap_position(id), Some(position as u32));
		}

		// An absent id has no position; the cached table survives repeated queries.
		let stranger = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"absent");
		assert!(midx.bitmap_position(&stranger).is_none());
		let four = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"four");
		assert_eq!(midx.bitmap_position(&four), Some(0));

		// A MIDX without a reverse index cannot answer bitmap positions at all.
		let plain =
			decode_multi_pack_index::<Sha256>(&encode_multi_pack_index(&names, &entries).unwrap())
				.expect("decode plain");
		assert!(plain.bitmap_position(&four).is_none());
	}

	#[test]
	fn reverse_index_dedup_prefers_the_selected_pack() {
		// An object in both pack 0 and pack 1; with pack 1 preferred, the MIDX must resolve it to
		// pack 1 (git's rule) rather than the lowest pack id.
		let names = vec!["pack-a.pack".to_owned(), "pack-b.pack".to_owned()];
		let dup = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"shared");
		let entries = vec![
			MidxEntry {
				id: dup,
				pack_id: 0,
				offset: 12,
			},
			MidxEntry {
				id: dup,
				pack_id: 1,
				offset: 99,
			},
		];
		let bytes = encode_multi_pack_index_with_reverse_index(&names, &entries, 1).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		assert_eq!(
			midx.lookup(&dup),
			Some((1, 99)),
			"resolved to the preferred pack"
		);
		// Without a preferred pack the lowest pack id still wins.
		let plain =
			decode_multi_pack_index::<Sha256>(&encode_multi_pack_index(&names, &entries).unwrap())
				.expect("decode plain");
		assert_eq!(plain.lookup(&dup), Some((0, 12)));
	}

	#[test]
	fn dedups_an_id_keeping_the_lowest_pack() {
		// The same object in two packs must be listed once, pointing at the lower pack id.
		let names = vec!["pack-a.pack".to_owned(), "pack-b.pack".to_owned()];
		let dup = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"shared");
		let entries = vec![
			MidxEntry {
				id: dup,
				pack_id: 1,
				offset: 99,
			},
			MidxEntry {
				id: dup,
				pack_id: 0,
				offset: 12,
			},
		];
		let bytes = encode_multi_pack_index(&names, &entries).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		assert_eq!(midx.len(), 1);
		assert_eq!(midx.lookup(&dup), Some((0, 12)));
	}

	#[test]
	fn round_trips_large_offsets() {
		// An offset above 2^32 forces the 64-bit LOFF chunk.
		let names = vec!["pack-a.pack".to_owned()];
		let entries = vec![
			entry::<Sha256>(b"small", 0, 100),
			entry::<Sha256>(b"huge", 0, 0x1_2345_6789),
		];
		let bytes = encode_multi_pack_index(&names, &entries).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		for e in &entries {
			assert_eq!(midx.lookup(&e.id), Some((0, e.offset)));
		}
	}

	#[test]
	fn round_trips_a_high_bit_offset_without_loff() {
		// An offset in [2^31, 2^32) with no offset ≥ 2^32: git (and we) store it inline in full,
		// with no LOFF chunk. The high bit is data, not a LOFF flag — the case codex flagged.
		let names = vec!["pack-a.pack".to_owned()];
		let entries = vec![
			entry::<Sha256>(b"low", 0, 10),
			entry::<Sha256>(b"twoish_gib", 0, 0x9000_0000),
		];
		let bytes = encode_multi_pack_index(&names, &entries).expect("encode");
		let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode");
		assert_eq!(
			midx
				.lookup(&ObjectId::<Sha256>::compute(
					ObjectKind::Blob,
					b"twoish_gib"
				))
				.map(|(_, off)| off),
			Some(0x9000_0000),
		);
	}

	#[test]
	fn round_trips_under_sha1() {
		let names = vec!["pack-x.pack".to_owned(), "pack-y.pack".to_owned()];
		let entries = vec![entry::<Sha1>(b"a", 0, 12), entry::<Sha1>(b"b", 1, 34)];
		let bytes = encode_multi_pack_index(&names, &entries).expect("encode");
		let midx = decode_multi_pack_index::<Sha1>(&bytes).expect("decode");
		assert_eq!(midx.len(), 2);
		for e in &entries {
			assert_eq!(midx.lookup(&e.id), Some((e.pack_id as usize, e.offset)));
		}
		// A SHA-1 MIDX is not a SHA-256 one (hash-version byte differs).
		assert!(decode_multi_pack_index::<Sha256>(&bytes).is_err());
	}

	#[test]
	fn rejects_unsorted_pack_names_and_bad_pack_ids() {
		let unsorted = vec!["pack-b.pack".to_owned(), "pack-a.pack".to_owned()];
		assert!(matches!(
			encode_multi_pack_index(&unsorted, &[entry::<Sha256>(b"x", 0, 1)]),
			Err(ObjectError::MalformedMultiPackIndex)
		));
		let names = vec!["pack-a.pack".to_owned()];
		assert!(matches!(
			encode_multi_pack_index(&names, &[entry::<Sha256>(b"x", 3, 1)]),
			Err(ObjectError::MalformedMultiPackIndex)
		));
	}

	#[test]
	fn rejects_a_corrupt_index() {
		let names = vec!["pack-a.pack".to_owned()];
		let bytes = encode_multi_pack_index(&names, &[entry::<Sha256>(b"x", 0, 1)]).expect("encode");
		let mut corrupt = bytes.clone();
		let last = corrupt.len() - 1;
		corrupt[last] ^= 0xff; // break the trailing checksum
		assert!(matches!(
			decode_multi_pack_index::<Sha256>(&corrupt),
			Err(ObjectError::MalformedMultiPackIndex)
		));
		let mut bad_magic = bytes;
		bad_magic[0] ^= 0xff;
		assert!(matches!(
			decode_multi_pack_index::<Sha256>(&bad_magic),
			Err(ObjectError::MalformedMultiPackIndex)
		));
	}
}
