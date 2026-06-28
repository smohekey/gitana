//! Reader for git packfiles (v2), resolving OFS and REF deltas.
//!
//! Decodes a self-contained pack into fully materialised objects with their
//! SHA-256 ids. Thin packs (REF deltas whose base is not in the pack) are
//! rejected with [`ObjectError::UnresolvedDeltaBase`]. The pack trailer hash is
//! verified before any entry is read.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::loose::MAX_OBJECT_SIZE;
use crate::{ObjectError, ObjectId, ObjectKind, apply_delta};

const HEADER_LEN: usize = 12;
const TRAILER_LEN: usize = 32;

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// A fully materialised object decoded from a packfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedObject {
	/// The object's SHA-256 id.
	pub id: ObjectId,
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
pub fn decode_pack(bytes: &[u8]) -> Result<Vec<PackedObject>, ObjectError> {
	decode_pack_with_bases(bytes, &HashMap::new())
}

/// Decode a packfile, resolving REF deltas whose base is not in the pack from
/// `external_bases` (an id → `(kind, payload)` map).
///
/// This is how a **thin** pack is read: `git push`/`fetch` may omit base objects the
/// peer already has and reference them by id. The caller supplies those bases (e.g.
/// read from the object store). A REF delta whose base is in neither the pack nor
/// `external_bases` still fails with [`ObjectError::UnresolvedDeltaBase`].
pub fn decode_pack_with_bases(
	bytes: &[u8],
	external_bases: &HashMap<ObjectId, (ObjectKind, Vec<u8>)>,
) -> Result<Vec<PackedObject>, ObjectError> {
	if bytes.len() < HEADER_LEN + TRAILER_LEN {
		return Err(ObjectError::MalformedPack);
	}
	if &bytes[0..4] != b"PACK" || read_u32(&bytes[4..8]) != 2 {
		return Err(ObjectError::MalformedPack);
	}
	let count = read_u32(&bytes[8..12]) as usize;

	let body_end = bytes.len() - TRAILER_LEN;
	let expected_trailer = &bytes[body_end..];
	if Sha256::digest(&bytes[..body_end]).as_slice() != expected_trailer {
		return Err(ObjectError::MalformedPack);
	}

	let mut objects: Vec<PackedObject> = Vec::with_capacity(count);
	let mut by_offset: HashMap<usize, usize> = HashMap::with_capacity(count);
	let mut by_id: HashMap<ObjectId, usize> = HashMap::with_capacity(count);

	let mut cursor = HEADER_LEN;
	for _ in 0..count {
		let entry_start = cursor;
		let (raw_type, size) = read_object_header(bytes, &mut cursor)?;

		let (kind, data) = match raw_type {
			OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
				let (data, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				(kind_of(raw_type)?, data)
			}
			OBJ_OFS_DELTA => {
				let distance = read_offset(bytes, &mut cursor)?;
				let base_start = entry_start
					.checked_sub(distance)
					.ok_or(ObjectError::MalformedPack)?;
				let base_index = *by_offset
					.get(&base_start)
					.ok_or(ObjectError::MalformedPack)?;
				let (delta, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				let base = &objects[base_index];
				(base.kind, apply_delta(&base.data, &delta)?)
			}
			OBJ_REF_DELTA => {
				let base_id = read_object_id(bytes, &mut cursor)?;
				let (delta, consumed) = inflate(&bytes[cursor..body_end], size)?;
				cursor += consumed;
				if let Some(&base_index) = by_id.get(&base_id) {
					let base = &objects[base_index];
					(base.kind, apply_delta(&base.data, &delta)?)
				} else if let Some((kind, data)) = external_bases.get(&base_id) {
					(*kind, apply_delta(data, &delta)?)
				} else {
					return Err(ObjectError::UnresolvedDeltaBase);
				}
			}
			_ => return Err(ObjectError::MalformedPack),
		};

		let id = ObjectId::compute(kind, &data);
		let index = objects.len();
		by_offset.insert(entry_start, index);
		by_id.insert(id, index);
		objects.push(PackedObject { id, kind, data });
	}

	if cursor != body_end {
		return Err(ObjectError::MalformedPack);
	}
	Ok(objects)
}

/// The base ids referenced by a pack's REF-delta entries, without resolving them.
///
/// Lets a thin-pack receiver pre-fetch the bases it must supply to
/// [`decode_pack_with_bases`]. Bases that are themselves in the pack are still
/// listed; the caller simply won't find them in its store, which is harmless.
pub fn ref_delta_base_ids(bytes: &[u8]) -> Result<Vec<ObjectId>, ObjectError> {
	if bytes.len() < HEADER_LEN + TRAILER_LEN {
		return Err(ObjectError::MalformedPack);
	}
	if &bytes[0..4] != b"PACK" || read_u32(&bytes[4..8]) != 2 {
		return Err(ObjectError::MalformedPack);
	}
	let count = read_u32(&bytes[8..12]) as usize;
	let body_end = bytes.len() - TRAILER_LEN;

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
				ids.push(read_object_id(bytes, &mut cursor)?);
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

fn read_object_id(bytes: &[u8], cursor: &mut usize) -> Result<ObjectId, ObjectError> {
	let end = cursor.checked_add(32).ok_or(ObjectError::MalformedPack)?;
	let raw = bytes.get(*cursor..end).ok_or(ObjectError::MalformedPack)?;
	let mut id = [0u8; 32];
	id.copy_from_slice(raw);
	*cursor = end;
	Ok(ObjectId::from_bytes(id))
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

	fn finish_pack(body: &[u8], count: u32) -> Vec<u8> {
		let mut pack = Vec::new();
		pack.extend_from_slice(b"PACK");
		pack.extend_from_slice(&2u32.to_be_bytes());
		pack.extend_from_slice(&count.to_be_bytes());
		pack.extend_from_slice(body);
		let trailer = Sha256::digest(&pack);
		pack.extend_from_slice(&trailer);
		pack
	}

	#[test]
	fn decodes_base_and_both_delta_kinds() {
		let base_payload = b"hello world";
		let base_id = ObjectId::compute(ObjectKind::Blob, base_payload);

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

		let pack = finish_pack(&body, 3);
		let objects = decode_pack(&pack).expect("decode");

		assert_eq!(objects.len(), 3);
		assert_eq!(objects[0].data, b"hello world");
		assert_eq!(objects[0].id, base_id);
		assert_eq!(objects[1].data, b"hello!!!");
		assert_eq!(
			objects[1].id,
			ObjectId::compute(ObjectKind::Blob, b"hello!!!")
		);
		assert_eq!(objects[2].data, b"world?");
		assert_eq!(objects[2].kind, ObjectKind::Blob);
	}

	#[test]
	fn rejects_bad_trailer() {
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_BLOB, 1));
		body.extend(zlib(b"x"));
		let mut pack = finish_pack(&body, 1);
		let last = pack.len() - 1;
		pack[last] ^= 0xff; // corrupt the trailer
		assert!(matches!(
			decode_pack(&pack),
			Err(ObjectError::MalformedPack)
		));
	}

	#[test]
	fn rejects_thin_pack_ref_delta() {
		let unknown = ObjectId::compute(ObjectKind::Blob, b"not in pack");
		let d = delta(11, 0, 1, b"");
		let mut body = Vec::new();
		body.extend(obj_header(OBJ_REF_DELTA, d.len()));
		body.extend(unknown.as_bytes());
		body.extend(zlib(&d));
		let pack = finish_pack(&body, 1);
		assert!(matches!(
			decode_pack(&pack),
			Err(ObjectError::UnresolvedDeltaBase)
		));
	}
}
