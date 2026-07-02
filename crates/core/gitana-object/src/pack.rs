//! Reader for git packfiles (v2), resolving OFS and REF deltas.
//!
//! Decodes a self-contained pack into fully materialised objects with their ids under
//! the hash algorithm `H`. Deltas resolve regardless of order, so a REF delta may precede
//! its base in the pack (as `git index-pack` allows). Thin packs (REF deltas whose base is
//! not in the pack) are rejected with [`ObjectError::UnresolvedDeltaBase`]. The pack
//! trailer hash is verified before any entry is read.

use std::collections::{HashMap, HashSet};

use crate::loose::MAX_OBJECT_SIZE;
use crate::{
	HashAlgorithm, ObjectError, ObjectId, ObjectKind, PackIndex, PackIndexEntry, apply_delta,
};

const HEADER_LEN: usize = 12;

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// A fully materialised object decoded from a packfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedObject<H: HashAlgorithm> {
	/// The object's id.
	pub id: ObjectId<H>,
	/// The object kind.
	pub kind: ObjectKind,
	/// The object payload (deltas already applied).
	pub data: Vec<u8>,
}

/// Decode a self-contained packfile into its objects, resolving all deltas.
///
/// Rejects thin packs (REF deltas whose base is absent) with
/// [`ObjectError::UnresolvedDeltaBase`]; use [`decode_pack_with_bases`] to resolve a
/// thin pack against objects held elsewhere.
pub fn decode_pack<H: HashAlgorithm>(bytes: &[u8]) -> Result<Vec<PackedObject<H>>, ObjectError> {
	decode_pack_with_bases(bytes, &HashMap::new())
}

/// Decode a packfile, resolving REF deltas whose base is not in the pack from
/// `external_bases` (an id → `(kind, payload)` map).
///
/// This is how a **thin** pack is read: `git push`/`fetch` may omit base objects the
/// peer already has and reference them by id. The caller supplies those bases (e.g.
/// read from the object store). A REF delta whose base is in neither the pack nor
/// `external_bases` still fails with [`ObjectError::UnresolvedDeltaBase`].
pub fn decode_pack_with_bases<H: HashAlgorithm>(
	bytes: &[u8],
	external_bases: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
) -> Result<Vec<PackedObject<H>>, ObjectError> {
	Ok(
		decode_entries(bytes, external_bases)?
			.into_iter()
			.map(|entry| entry.object)
			.collect(),
	)
}

/// The `.idx` entries for a self-contained pack: each object's id, byte offset, and the
/// CRC-32 of its packed bytes. Feeds [`crate::encode_pack_index`]. Resolves deltas to
/// recover ids (like [`decode_pack`]), so a thin pack is rejected with
/// [`ObjectError::UnresolvedDeltaBase`].
pub fn pack_index_entries<H: HashAlgorithm>(
	bytes: &[u8],
) -> Result<Vec<PackIndexEntry<H>>, ObjectError> {
	Ok(
		decode_entries(bytes, &HashMap::new())?
			.into_iter()
			.map(|entry| PackIndexEntry {
				id: entry.object.id,
				offset: entry.offset,
				crc32: entry.crc32,
			})
			.collect(),
	)
}

/// Decode a single object at byte `offset` within a self-contained pack, resolving only its
/// own delta chain (not the whole pack). `index` (the pack's `.idx`) locates REF-delta bases
/// by id; OFS-delta bases are found by their relative offset. This is the random-access read
/// path: with a `.idx` to map id → offset, an object is materialised without decoding every
/// other object in the pack.
///
/// A REF-delta base absent from `index` fails with [`ObjectError::UnresolvedDeltaBase`] (our
/// stored packs are self-contained, so every base is in the same pack). A delta chain that
/// revisits an offset (a cycle) or a structurally malformed entry fails with
/// [`ObjectError::MalformedPack`]. The pack's trailer hash is *not* re-verified here (that is
/// O(pack) per read); the returned id is recomputed from the materialised bytes, so a
/// corrupt object is still caught by its content address.
pub fn decode_object_at<H: HashAlgorithm>(
	pack: &[u8],
	index: &PackIndex<H>,
	offset: u64,
) -> Result<PackedObject<H>, ObjectError> {
	let trailer_len = H::RAW_LEN;
	if pack.len() < HEADER_LEN + trailer_len {
		return Err(ObjectError::MalformedPack);
	}
	if &pack[0..4] != b"PACK" || read_u32(&pack[4..8]) != 2 {
		return Err(ObjectError::MalformedPack);
	}
	let body_end = pack.len() - trailer_len;

	// Walk from `offset` down the delta chain to a base object, collecting each delta
	// outermost-first, then apply them base-outward. `visited` catches a REF-delta cycle
	// (OFS bases point strictly earlier and cannot cycle, but a REF base may point anywhere).
	let mut deltas: Vec<Vec<u8>> = Vec::new();
	let mut visited: HashSet<u64> = HashSet::new();
	let mut cursor_offset = offset;
	let (kind, mut data) = loop {
		if !visited.insert(cursor_offset) {
			return Err(ObjectError::MalformedPack);
		}
		match read_raw_entry::<H>(pack, body_end, cursor_offset, index)? {
			RawEntry::Base { kind, data } => break (kind, data),
			RawEntry::Delta { base_offset, delta } => {
				deltas.push(delta);
				cursor_offset = base_offset;
			}
		}
	};
	for delta in deltas.iter().rev() {
		data = apply_delta(&data, delta)?;
	}
	let id = ObjectId::<H>::compute(kind, &data);
	Ok(PackedObject { id, kind, data })
}

/// One pack entry read in isolation for [`decode_object_at`]: either a materialised base
/// object, or a delta plus the byte offset of the base it applies to.
enum RawEntry {
	Base { kind: ObjectKind, data: Vec<u8> },
	Delta { base_offset: u64, delta: Vec<u8> },
}

/// Read the single entry at `offset`: inflate a base object, or return a delta with the byte
/// offset of its base (an OFS distance resolved against `offset`, or a REF id resolved
/// through `index`). Does not recurse — [`decode_object_at`] drives the chain.
fn read_raw_entry<H: HashAlgorithm>(
	pack: &[u8],
	body_end: usize,
	offset: u64,
	index: &PackIndex<H>,
) -> Result<RawEntry, ObjectError> {
	let entry_start = usize::try_from(offset).map_err(|_| ObjectError::MalformedPack)?;
	if entry_start < HEADER_LEN || entry_start >= body_end {
		return Err(ObjectError::MalformedPack);
	}
	let mut cursor = entry_start;
	let (raw_type, size) = read_object_header(pack, &mut cursor)?;
	match raw_type {
		OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
			let (data, _) = inflate(&pack[cursor..body_end], size)?;
			Ok(RawEntry::Base {
				kind: kind_of(raw_type)?,
				data,
			})
		}
		OBJ_OFS_DELTA => {
			let distance = read_offset(pack, &mut cursor)?;
			let base_start = entry_start
				.checked_sub(distance)
				.ok_or(ObjectError::MalformedPack)?;
			// OFS bases point strictly earlier, into the body (never the header).
			if base_start < HEADER_LEN || base_start >= entry_start {
				return Err(ObjectError::MalformedPack);
			}
			let (delta, _) = inflate(&pack[cursor..body_end], size)?;
			Ok(RawEntry::Delta {
				base_offset: base_start as u64,
				delta,
			})
		}
		OBJ_REF_DELTA => {
			let base_id = read_object_id::<H>(pack, &mut cursor)?;
			let base_offset = index
				.offset_of(&base_id)
				.ok_or(ObjectError::UnresolvedDeltaBase)?;
			let (delta, _) = inflate(&pack[cursor..body_end], size)?;
			Ok(RawEntry::Delta { base_offset, delta })
		}
		_ => Err(ObjectError::MalformedPack),
	}
}

/// One packfile entry decoded from just its own bytes: a whole object, or a delta together with an
/// *unresolved* reference to its base — an OFS `distance` (subtract from this entry's own byte
/// offset) or a REF `base` id. Leaving the base unresolved lets a caller fetch each object's bytes
/// lazily (e.g. one `read_path_range` per entry) rather than holding the whole pack in memory.
pub enum PackEntry<H: HashAlgorithm> {
	/// A complete object.
	Base {
		/// The object kind.
		kind: ObjectKind,
		/// The object payload.
		data: Vec<u8>,
	},
	/// A delta against the base `distance` bytes earlier in the pack.
	OfsDelta {
		/// Byte distance from this entry's offset back to its base.
		distance: u64,
		/// The delta instructions.
		delta: Vec<u8>,
	},
	/// A delta against the base object named by `base`.
	RefDelta {
		/// The base object's id.
		base: ObjectId<H>,
		/// The delta instructions.
		delta: Vec<u8>,
	},
}

/// Decode a single packfile entry from `entry` — a slice that begins at the entry's byte offset
/// and contains at least its full compressed data (e.g. the `[offset, next_offset)` span computed
/// from a `.idx`'s offsets). Returns the base object, or the delta with its unresolved base
/// reference; the caller resolves the base and applies the delta with [`apply_delta`]. Touches no
/// other part of the pack, so it underpins a lazy, memory-bounded read path.
pub fn decode_pack_entry<H: HashAlgorithm>(entry: &[u8]) -> Result<PackEntry<H>, ObjectError> {
	let mut cursor = 0;
	let (raw_type, size) = read_object_header(entry, &mut cursor)?;
	match raw_type {
		OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
			let (data, _) = inflate(&entry[cursor..], size)?;
			Ok(PackEntry::Base {
				kind: kind_of(raw_type)?,
				data,
			})
		}
		OBJ_OFS_DELTA => {
			let distance = read_offset(entry, &mut cursor)?;
			let (delta, _) = inflate(&entry[cursor..], size)?;
			Ok(PackEntry::OfsDelta {
				distance: distance as u64,
				delta,
			})
		}
		OBJ_REF_DELTA => {
			let base = read_object_id::<H>(entry, &mut cursor)?;
			let (delta, _) = inflate(&entry[cursor..], size)?;
			Ok(PackEntry::RefDelta { base, delta })
		}
		_ => Err(ObjectError::MalformedPack),
	}
}

/// A decoded pack entry: the materialised object plus its pack location — byte offset and
/// the CRC-32 of its packed bytes — everything both a plain decode and a `.idx` need.
struct DecodedEntry<H: HashAlgorithm> {
	object: PackedObject<H>,
	offset: u64,
	crc32: u32,
}

/// Apply `delta` to `base` (borrowed, never copied — a base can be up to `MAX_OBJECT_SIZE`)
/// and wrap the result as a materialised object of `kind`, computing its id.
fn apply_delta_object<H: HashAlgorithm>(
	kind: ObjectKind,
	base: &[u8],
	delta: &[u8],
) -> Result<PackedObject<H>, ObjectError> {
	let data = apply_delta(base, delta)?;
	let id = ObjectId::<H>::compute(kind, &data);
	Ok(PackedObject { id, kind, data })
}

/// Seed the resolve worklist with REF deltas whose base is only in `external_bases` (a thin
/// pack): such a base never enters the in-pack `ready` queue, so resolve its waiters here,
/// removing them from `waiting_by_id` and queueing each so its own dependents unblock in turn.
fn resolve_external_ref_deltas<H: HashAlgorithm>(
	external_bases: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	deltas: &[Option<Vec<u8>>],
	waiting_by_id: &mut HashMap<ObjectId<H>, Vec<usize>>,
	resolved: &mut [Option<PackedObject<H>>],
	ready: &mut Vec<usize>,
) -> Result<(), ObjectError> {
	for (base_id, (kind, data)) in external_bases {
		let Some(waiters) = waiting_by_id.remove(base_id) else {
			continue;
		};
		for waiter in waiters {
			if resolved[waiter].is_none() {
				let delta = deltas[waiter]
					.as_ref()
					.expect("a filed delta has its bytes");
				resolved[waiter] = Some(apply_delta_object(*kind, data, delta)?);
				ready.push(waiter);
			}
		}
	}
	Ok(())
}

/// Decode every entry of a self-contained pack: materialise each object and record its byte
/// offset and the CRC-32 of its packed bytes. Shared by [`decode_pack_with_bases`] and
/// [`pack_index_entries`]. The pack trailer hash is verified before any entry is read.
///
/// Resolution is two-pass so delta order does not matter: the scan pass materialises base
/// objects and files deltas under their base; a dependency worklist then resolves them in
/// one linear sweep, letting a REF delta's base appear *later* in the pack (which
/// `git index-pack` allows and a single forward pass would reject). OFS deltas still
/// reference a strictly earlier offset. A REF base found in neither the pack nor
/// `external_bases` (a thin pack), or a delta cycle, fails with
/// [`ObjectError::UnresolvedDeltaBase`]. Returned in pack order.
fn decode_entries<H: HashAlgorithm>(
	bytes: &[u8],
	external_bases: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
) -> Result<Vec<DecodedEntry<H>>, ObjectError> {
	let trailer_len = H::RAW_LEN;
	if bytes.len() < HEADER_LEN + trailer_len {
		return Err(ObjectError::MalformedPack);
	}
	if &bytes[0..4] != b"PACK" || read_u32(&bytes[4..8]) != 2 {
		return Err(ObjectError::MalformedPack);
	}
	let count = read_u32(&bytes[8..12]) as usize;

	let body_end = bytes.len() - trailer_len;
	let expected_trailer = &bytes[body_end..];
	if H::digest(&[&bytes[..body_end]]).as_ref() != expected_trailer {
		return Err(ObjectError::MalformedPack);
	}

	// Scan pass: parse every entry's location (byte offset + CRC-32). Base objects are
	// materialised at once and queued in `ready`; deltas are filed under the base they wait
	// for — `waiting_by_offset` for OFS (an earlier byte offset), `waiting_by_id` for REF (an
	// id that may lie later in the pack or in `external_bases`). `deltas[i]` holds the raw delta.
	let mut resolved: Vec<Option<PackedObject<H>>> = (0..count).map(|_| None).collect();
	let mut deltas: Vec<Option<Vec<u8>>> = (0..count).map(|_| None).collect();
	let mut offsets: Vec<usize> = Vec::with_capacity(count);
	let mut crcs: Vec<u32> = Vec::with_capacity(count);
	let mut by_offset: HashMap<usize, usize> = HashMap::with_capacity(count);
	let mut waiting_by_offset: HashMap<usize, Vec<usize>> = HashMap::new();
	let mut waiting_by_id: HashMap<ObjectId<H>, Vec<usize>> = HashMap::new();
	let mut ready: Vec<usize> = Vec::new();

	let mut cursor = HEADER_LEN;
	for index in 0..count {
		let entry_start = cursor;
		let (raw_type, size) = read_object_header(bytes, &mut cursor)?;

		match raw_type {
			OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
				let (data, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				let kind = kind_of(raw_type)?;
				let id = ObjectId::<H>::compute(kind, &data);
				resolved[index] = Some(PackedObject { id, kind, data });
				ready.push(index);
			}
			OBJ_OFS_DELTA => {
				let distance = read_offset(bytes, &mut cursor)?;
				let base_start = entry_start
					.checked_sub(distance)
					.ok_or(ObjectError::MalformedPack)?;
				// OFS bases point strictly earlier, so a valid one is already in `by_offset`.
				if base_start >= entry_start || !by_offset.contains_key(&base_start) {
					return Err(ObjectError::MalformedPack);
				}
				let (delta, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				deltas[index] = Some(delta);
				waiting_by_offset.entry(base_start).or_default().push(index);
			}
			OBJ_REF_DELTA => {
				let base_id = read_object_id::<H>(bytes, &mut cursor)?;
				let (delta, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				deltas[index] = Some(delta);
				waiting_by_id.entry(base_id).or_default().push(index);
			}
			_ => return Err(ObjectError::MalformedPack),
		}

		by_offset.insert(entry_start, index);
		let mut crc = flate2::Crc::new();
		crc.update(&bytes[entry_start..cursor]);
		offsets.push(entry_start);
		crcs.push(crc.sum());
	}

	if cursor != body_end {
		return Err(ObjectError::MalformedPack);
	}

	// Resolve pass: a worklist over materialised objects. Popping one unblocks exactly the
	// deltas waiting on its byte offset (OFS) or its id (REF), and each resolved delta is
	// itself pushed, so a chain unwinds in a single linear sweep whatever its layout. The base
	// is moved out while its (up to MAX_OBJECT_SIZE) payload feeds the deltas, then restored —
	// no per-delta copy of the base.
	resolve_external_ref_deltas(
		external_bases,
		&deltas,
		&mut waiting_by_id,
		&mut resolved,
		&mut ready,
	)?;
	while let Some(index) = ready.pop() {
		let base = resolved[index]
			.take()
			.expect("a queued entry is materialised");
		let mut waiters = waiting_by_offset
			.remove(&offsets[index])
			.unwrap_or_default();
		if let Some(mut by_id) = waiting_by_id.remove(&base.id) {
			waiters.append(&mut by_id);
		}
		for waiter in waiters {
			if resolved[waiter].is_none() {
				let delta = deltas[waiter]
					.as_ref()
					.expect("a filed delta has its bytes");
				resolved[waiter] = Some(apply_delta_object(base.kind, &base.data, delta)?);
				ready.push(waiter);
			}
		}
		resolved[index] = Some(base);
	}

	// Emit in pack order. A slot still empty is a delta whose base is absent (a thin pack) or
	// part of a cycle — unresolvable.
	let mut entries = Vec::with_capacity(count);
	for (index, slot) in resolved.into_iter().enumerate() {
		let object = slot.ok_or(ObjectError::UnresolvedDeltaBase)?;
		entries.push(DecodedEntry {
			object,
			offset: offsets[index] as u64,
			crc32: crcs[index],
		});
	}
	Ok(entries)
}

/// The base ids referenced by a pack's REF-delta entries, without resolving them.
///
/// Lets a thin-pack receiver pre-fetch the bases it must supply to
/// [`decode_pack_with_bases`]. Bases that are themselves in the pack are still
/// listed; the caller simply won't find them in its store, which is harmless.
pub fn ref_delta_base_ids<H: HashAlgorithm>(bytes: &[u8]) -> Result<Vec<ObjectId<H>>, ObjectError> {
	let trailer_len = H::RAW_LEN;
	if bytes.len() < HEADER_LEN + trailer_len {
		return Err(ObjectError::MalformedPack);
	}
	if &bytes[0..4] != b"PACK" || read_u32(&bytes[4..8]) != 2 {
		return Err(ObjectError::MalformedPack);
	}
	let count = read_u32(&bytes[8..12]) as usize;
	let body_end = bytes.len() - trailer_len;

	let mut ids = Vec::new();
	let mut cursor = HEADER_LEN;
	for _ in 0..count {
		let (raw_type, size) = read_object_header(bytes, &mut cursor)?;
		match raw_type {
			OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
				let (_, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
			}
			OBJ_OFS_DELTA => {
				read_offset(bytes, &mut cursor)?;
				let (_, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
			}
			OBJ_REF_DELTA => {
				ids.push(read_object_id::<H>(bytes, &mut cursor)?);
				let (_, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
			}
			_ => return Err(ObjectError::MalformedPack),
		}
	}
	Ok(ids)
}

fn kind_of(raw_type: u8) -> Result<ObjectKind, ObjectError> {
	match raw_type {
		OBJ_COMMIT => Ok(ObjectKind::Commit),
		OBJ_TREE => Ok(ObjectKind::Tree),
		OBJ_BLOB => Ok(ObjectKind::Blob),
		OBJ_TAG => Ok(ObjectKind::Tag),
		_ => Err(ObjectError::MalformedPack),
	}
}

/// Read the per-entry `type` + uncompressed `size` header.
fn read_object_header(bytes: &[u8], cursor: &mut usize) -> Result<(u8, usize), ObjectError> {
	let first = next(bytes, cursor)?;
	let raw_type = (first >> 4) & 0x07;
	let mut size = (first & 0x0f) as usize;
	let mut shift = 4u32;
	let mut byte = first;
	while byte & 0x80 != 0 {
		byte = next(bytes, cursor)?;
		let part = (byte & 0x7f) as usize;
		size = size
			.checked_add(part.checked_shl(shift).ok_or(ObjectError::MalformedPack)?)
			.ok_or(ObjectError::MalformedPack)?;
		shift += 7;
	}
	Ok((raw_type, size))
}

/// Read the OFS-delta base distance (big-endian base-128, offset-encoded).
fn read_offset(bytes: &[u8], cursor: &mut usize) -> Result<usize, ObjectError> {
	let mut byte = next(bytes, cursor)?;
	let mut offset = (byte & 0x7f) as usize;
	while byte & 0x80 != 0 {
		byte = next(bytes, cursor)?;
		offset = offset
			.checked_add(1)
			.and_then(|o| o.checked_shl(7))
			.ok_or(ObjectError::MalformedPack)?
			| (byte & 0x7f) as usize;
	}
	Ok(offset)
}

fn read_object_id<H: HashAlgorithm>(
	bytes: &[u8],
	cursor: &mut usize,
) -> Result<ObjectId<H>, ObjectError> {
	let end = cursor
		.checked_add(H::RAW_LEN)
		.ok_or(ObjectError::MalformedPack)?;
	let raw = bytes.get(*cursor..end).ok_or(ObjectError::MalformedPack)?;
	let id = ObjectId::from_bytes(raw)?;
	*cursor = end;
	Ok(id)
}

/// Inflate one zlib stream, returning the data and the number of input bytes used.
fn inflate(input: &[u8], expected: usize) -> Result<(Vec<u8>, usize), ObjectError> {
	let mut decompress = flate2::Decompress::new(true);
	let mut out = Vec::with_capacity(expected.min(MAX_OBJECT_SIZE as usize) + 16);
	loop {
		let consumed = decompress.total_in() as usize;
		let status = decompress
			.decompress_vec(
				&input[consumed..],
				&mut out,
				flate2::FlushDecompress::Finish,
			)
			.map_err(|error| ObjectError::Zlib(error.to_string()))?;
		if out.len() as u64 > MAX_OBJECT_SIZE {
			return Err(ObjectError::TooLarge);
		}
		match status {
			flate2::Status::StreamEnd => break,
			flate2::Status::Ok | flate2::Status::BufError => {
				if decompress.total_in() as usize >= input.len() && out.len() >= expected {
					return Err(ObjectError::Zlib("truncated zlib stream".to_owned()));
				}
				out.reserve(expected.max(64));
			}
		}
	}
	if out.len() != expected {
		return Err(ObjectError::MalformedPack);
	}
	Ok((out, decompress.total_in() as usize))
}

fn next(bytes: &[u8], cursor: &mut usize) -> Result<u8, ObjectError> {
	let byte = *bytes.get(*cursor).ok_or(ObjectError::MalformedPack)?;
	*cursor += 1;
	Ok(byte)
}

fn read_u32(bytes: &[u8]) -> u32 {
	u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
	use std::io::Write;

	use flate2::Compression;
	use flate2::write::ZlibEncoder;

	use super::*;
	use crate::{Sha1, Sha256};

	fn zlib(data: &[u8]) -> Vec<u8> {
		let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
		encoder.write_all(data).unwrap();
		encoder.finish().unwrap()
	}

	fn obj_header(obj_type: u8, size: usize) -> Vec<u8> {
		let mut out = vec![(obj_type << 4) | (size & 0x0f) as u8];
		let mut size = size >> 4;
		if size != 0 {
			out[0] |= 0x80;
		}
		while size != 0 {
			let mut byte = (size & 0x7f) as u8;
			size >>= 7;
			if size != 0 {
				byte |= 0x80;
			}
			out.push(byte);
		}
		out
	}

	fn encode_offset(mut value: usize) -> Vec<u8> {
		let mut bytes = vec![(value & 0x7f) as u8];
		value >>= 7;
		while value != 0 {
			value -= 1;
			bytes.push(0x80 | (value & 0x7f) as u8);
			value >>= 7;
		}
		bytes.reverse();
		bytes
	}

	fn encode_size(out: &mut Vec<u8>, mut size: usize) {
		loop {
			let mut byte = (size & 0x7f) as u8;
			size >>= 7;
			if size != 0 {
				byte |= 0x80;
			}
			out.push(byte);
			if size == 0 {
				break;
			}
		}
	}

	/// A delta: copy `copy_len` bytes from `copy_off`, then insert `insert`.
	fn delta(src_len: usize, copy_off: usize, copy_len: usize, insert: &[u8]) -> Vec<u8> {
		let mut d = Vec::new();
		encode_size(&mut d, src_len);
		encode_size(&mut d, copy_len + insert.len());
		d.push(0x80 | 0x01 | 0x10);
		d.push(copy_off as u8);
		d.push(copy_len as u8);
		d.push(insert.len() as u8);
		d.extend_from_slice(insert);
		d
	}

	/// Append a `PACK` header, `body`, and the `H` trailer hash.
	fn finish_pack<H: HashAlgorithm>(body: &[u8], count: u32) -> Vec<u8> {
		let mut pack = Vec::new();
		pack.extend_from_slice(b"PACK");
		pack.extend_from_slice(&2u32.to_be_bytes());
		pack.extend_from_slice(&count.to_be_bytes());
		pack.extend_from_slice(body);
		let trailer = H::digest(&[&pack]);
		pack.extend_from_slice(trailer.as_ref());
		pack
	}

	#[test]
	fn decodes_base_and_both_delta_kinds() {
		let base_payload = b"hello world";
		let base_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, base_payload);

		// B: OFS delta -> "hello!!!"; C: REF delta -> "world?"
		let b_delta = delta(base_payload.len(), 0, 5, b"!!!");
		let c_delta = delta(base_payload.len(), 6, 5, b"?");

		let mut body = Vec::new();
		let a_off = HEADER_LEN + body.len();
		body.extend(obj_header(OBJ_BLOB, base_payload.len()));
		body.extend(zlib(base_payload));

		let b_off = HEADER_LEN + body.len();
		body.extend(obj_header(OBJ_OFS_DELTA, b_delta.len()));
		body.extend(encode_offset(b_off - a_off));
		body.extend(zlib(&b_delta));

		body.extend(obj_header(OBJ_REF_DELTA, c_delta.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&c_delta));

		let pack = finish_pack::<Sha256>(&body, 3);
		let objects = decode_pack::<Sha256>(&pack).expect("decode");

		assert_eq!(objects.len(), 3);
		assert_eq!(objects[0].data, b"hello world");
		assert_eq!(objects[0].id, base_id);
		assert_eq!(objects[1].data, b"hello!!!");
		assert_eq!(
			objects[1].id,
			ObjectId::<Sha256>::compute(ObjectKind::Blob, b"hello!!!")
		);
		assert_eq!(objects[2].data, b"world?");
		assert_eq!(objects[2].kind, ObjectKind::Blob);
	}

	#[test]
	fn decodes_out_of_order_ref_delta_chain() {
		// A chain of REF deltas laid out back-to-front: D2 (index 0) → D1 (index 1) → O
		// (index 2). Neither delta's base precedes it, so the resolver must iterate — D1
		// resolves once O is known, then D2 once D1 is. git index-pack accepts this shape;
		// the old single forward pass rejected it with UnresolvedDeltaBase.
		let o = b"abcdefgh";
		let id_o = ObjectId::<Sha256>::compute(ObjectKind::Blob, o);
		let d1_out = b"abcdefghX";
		let id_d1 = ObjectId::<Sha256>::compute(ObjectKind::Blob, d1_out);

		let d1 = delta(o.len(), 0, o.len(), b"X");
		let d2 = delta(d1_out.len(), 0, d1_out.len(), b"Y");

		let mut body = Vec::new();
		body.extend(obj_header(OBJ_REF_DELTA, d2.len()));
		body.extend(id_d1.as_bytes());
		body.extend(zlib(&d2));
		body.extend(obj_header(OBJ_REF_DELTA, d1.len()));
		body.extend(id_o.as_bytes());
		body.extend(zlib(&d1));
		body.extend(obj_header(OBJ_BLOB, o.len()));
		body.extend(zlib(o));

		let pack = finish_pack::<Sha256>(&body, 3);
		let objects = decode_pack::<Sha256>(&pack).expect("decode out-of-order ref-delta chain");

		// Output stays in pack order, each object fully materialised.
		assert_eq!(objects.len(), 3);
		assert_eq!(objects[0].data, b"abcdefghXY");
		assert_eq!(objects[1].data, b"abcdefghX");
		assert_eq!(objects[1].id, id_d1);
		assert_eq!(objects[2].data, o);
		assert_eq!(objects[2].id, id_o);
	}

	#[test]
	fn decodes_sha1_ref_delta_pack() {
		// The same shape as above but under SHA-1: 20-byte ids and a 20-byte trailer.
		let base_payload = b"hello world";
		let base_id = ObjectId::<Sha1>::compute(ObjectKind::Blob, base_payload);
		let c_delta = delta(base_payload.len(), 6, 5, b"?");

		let mut body = Vec::new();
		body.extend(obj_header(OBJ_BLOB, base_payload.len()));
		body.extend(zlib(base_payload));
		body.extend(obj_header(OBJ_REF_DELTA, c_delta.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&c_delta));

		let pack = finish_pack::<Sha1>(&body, 2);
		let objects = decode_pack::<Sha1>(&pack).expect("decode");
		assert_eq!(objects.len(), 2);
		assert_eq!(objects[0].id, base_id);
		assert_eq!(objects[1].data, b"world?");

		let bases = ref_delta_base_ids::<Sha1>(&pack).expect("base ids");
		assert_eq!(bases, vec![base_id]);
	}

	#[test]
	fn rejects_bad_trailer() {
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_BLOB, 1));
		body.extend(zlib(b"x"));
		let mut pack = finish_pack::<Sha256>(&body, 1);
		let last = pack.len() - 1;
		pack[last] ^= 0xff; // corrupt the trailer
		assert!(matches!(
			decode_pack::<Sha256>(&pack),
			Err(ObjectError::MalformedPack)
		));
	}

	#[test]
	fn rejects_thin_pack_ref_delta() {
		let unknown = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"not in pack");
		let d = delta(11, 0, 1, b"");
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_REF_DELTA, d.len()));
		body.extend(unknown.as_bytes());
		body.extend(zlib(&d));
		let pack = finish_pack::<Sha256>(&body, 1);
		assert!(matches!(
			decode_pack::<Sha256>(&pack),
			Err(ObjectError::UnresolvedDeltaBase)
		));
	}

	/// Build an in-memory index for a pack, as the object store does from a `.idx` sidecar.
	fn index_of<H: HashAlgorithm>(pack: &[u8]) -> PackIndex<H> {
		let entries = pack_index_entries::<H>(pack).expect("index entries");
		let checksum = pack[pack.len() - H::RAW_LEN..].to_vec();
		PackIndex::from_entries(entries, checksum).expect("build index")
	}

	#[test]
	fn decode_pack_entry_reads_base_ofs_and_ref() {
		let base = b"hello world";

		let mut base_entry = obj_header(OBJ_BLOB, base.len());
		base_entry.extend(zlib(base));
		match decode_pack_entry::<Sha256>(&base_entry).expect("base") {
			PackEntry::Base { kind, data } => {
				assert_eq!(kind, ObjectKind::Blob);
				assert_eq!(data, base);
			}
			_ => panic!("expected a base entry"),
		}

		let ofs = delta(base.len(), 0, 5, b"!!!");
		let mut ofs_entry = obj_header(OBJ_OFS_DELTA, ofs.len());
		ofs_entry.extend(encode_offset(42));
		ofs_entry.extend(zlib(&ofs));
		match decode_pack_entry::<Sha256>(&ofs_entry).expect("ofs") {
			PackEntry::OfsDelta { distance, delta } => {
				assert_eq!(distance, 42);
				assert_eq!(delta, ofs);
			}
			_ => panic!("expected an OFS delta"),
		}

		let base_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, base);
		let refd = delta(base.len(), 6, 5, b"?");
		let mut ref_entry = obj_header(OBJ_REF_DELTA, refd.len());
		ref_entry.extend(base_id.as_bytes());
		ref_entry.extend(zlib(&refd));
		match decode_pack_entry::<Sha256>(&ref_entry).expect("ref") {
			PackEntry::RefDelta { base, delta } => {
				assert_eq!(base, base_id);
				assert_eq!(delta, refd);
			}
			_ => panic!("expected a REF delta"),
		}
	}

	#[test]
	fn decode_object_at_reads_base_and_both_deltas() {
		// Same pack shape as `decodes_base_and_both_delta_kinds`: a blob base, an OFS delta,
		// and a REF delta. Each object is read individually by its offset from the index.
		let base_payload = b"hello world";
		let base_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, base_payload);
		let b_delta = delta(base_payload.len(), 0, 5, b"!!!");
		let c_delta = delta(base_payload.len(), 6, 5, b"?");

		let mut body = Vec::new();
		let a_off = HEADER_LEN + body.len();
		body.extend(obj_header(OBJ_BLOB, base_payload.len()));
		body.extend(zlib(base_payload));
		let b_off = HEADER_LEN + body.len();
		body.extend(obj_header(OBJ_OFS_DELTA, b_delta.len()));
		body.extend(encode_offset(b_off - a_off));
		body.extend(zlib(&b_delta));
		body.extend(obj_header(OBJ_REF_DELTA, c_delta.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&c_delta));
		let pack = finish_pack::<Sha256>(&body, 3);

		let index = index_of::<Sha256>(&pack);
		let read = |id: &ObjectId<Sha256>| {
			let offset = index.offset_of(id).expect("in index");
			decode_object_at(&pack, &index, offset).expect("decode object")
		};

		let base = read(&base_id);
		assert_eq!(base.data, b"hello world");
		assert_eq!(base.id, base_id);

		let ofs_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"hello!!!");
		let ofs = read(&ofs_id);
		assert_eq!(ofs.data, b"hello!!!");
		assert_eq!(ofs.kind, ObjectKind::Blob);

		let ref_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"world?");
		let picked = read(&ref_id);
		assert_eq!(picked.data, b"world?");
		assert_eq!(picked.id, ref_id);
	}

	#[test]
	fn decode_object_at_reads_sha1_ref_delta() {
		// A base plus a REF delta under SHA-1 (20-byte ids/trailer), read one at a time.
		let base_payload = b"hello world";
		let base_id = ObjectId::<Sha1>::compute(ObjectKind::Blob, base_payload);
		let c_delta = delta(base_payload.len(), 6, 5, b"?");
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_BLOB, base_payload.len()));
		body.extend(zlib(base_payload));
		body.extend(obj_header(OBJ_REF_DELTA, c_delta.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&c_delta));
		let pack = finish_pack::<Sha1>(&body, 2);

		let index = index_of::<Sha1>(&pack);
		let ref_id = ObjectId::<Sha1>::compute(ObjectKind::Blob, b"world?");
		let offset = index.offset_of(&ref_id).expect("in index");
		let picked = decode_object_at(&pack, &index, offset).expect("decode");
		assert_eq!(picked.data, b"world?");
		assert_eq!(picked.id, ref_id);
	}

	#[test]
	fn decode_object_at_rejects_ref_delta_cycle() {
		// A lone REF delta whose base id is mapped (by a hand-built index) back to the delta's
		// own offset — a self-cycle. A well-formed pack can't express this (ids are content
		// derived), so drive it through the index the store would consult; the visited-offset
		// guard must reject it rather than loop.
		let base_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"anything");
		let d = delta(1, 0, 1, b"");
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_REF_DELTA, d.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&d));
		let pack = finish_pack::<Sha256>(&body, 1);

		let entry_offset = HEADER_LEN as u64;
		let index = PackIndex::from_entries(
			vec![PackIndexEntry {
				id: base_id,
				offset: entry_offset,
				crc32: 0,
			}],
			pack[pack.len() - 32..].to_vec(),
		)
		.expect("index");
		assert!(matches!(
			decode_object_at(&pack, &index, entry_offset),
			Err(ObjectError::MalformedPack)
		));
	}

	#[test]
	fn decode_object_at_rejects_unknown_ref_base() {
		// A REF delta whose base id is absent from the index (a base the pack does not carry).
		let base_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"missing base");
		let d = delta(1, 0, 1, b"");
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_REF_DELTA, d.len()));
		body.extend(base_id.as_bytes());
		body.extend(zlib(&d));
		let pack = finish_pack::<Sha256>(&body, 1);

		// An empty index carries no base, so the REF delta is unresolvable.
		let index = PackIndex::<Sha256>::from_entries(Vec::new(), pack[pack.len() - 32..].to_vec())
			.expect("index");
		assert!(matches!(
			decode_object_at(&pack, &index, HEADER_LEN as u64),
			Err(ObjectError::UnresolvedDeltaBase)
		));
	}
}
