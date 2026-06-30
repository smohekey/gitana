//! Writer for git packfiles (v2): the encoder counterpart to [`crate::decode_pack`].
//!
//! Serialises a set of fully materialised objects into a self-contained pack,
//! delta-compressing with a git-style sliding window (objects sorted by type then
//! size, each tried against a small window of earlier objects of the same type).
//! Deltas are emitted as `OBJ_OFS_DELTA` entries referencing an earlier entry by
//! byte distance, so the pack is never thin. The trailer is the `H` hash of the
//! whole pack. Round-trips through [`crate::decode_pack`] and is accepted by stock
//! `git index-pack` for the matching object format.

use std::collections::HashMap;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::pack::PackedObject;
use crate::{HashAlgorithm, ObjectKind};

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;

/// How many earlier objects each candidate is tried against for delta compression.
const WINDOW: usize = 10;
/// Cap on delta-chain depth, matching git's default, to bound resolution work.
const MAX_DEPTH: usize = 50;
/// Shortest base run worth encoding as a copy instruction.
const MIN_MATCH: usize = 4;
/// Largest run a single copy instruction can carry (size field is 3 bytes).
const MAX_COPY: usize = 0xff_ffff;
/// Largest literal a single insert instruction can carry (length field is 7 bits).
const MAX_INSERT: usize = 0x7f;
/// Cap on base offsets probed per target position, to keep encoding near-linear.
const MAX_CANDIDATES: usize = 64;

/// How one object is written into the pack.
enum Encoding {
	/// Full object: its zlib-compressed payload.
	Full,
	/// OFS delta against an earlier emitted entry (by position in the write order).
	Ofs {
		/// Index into the write order of the base entry.
		base_pos: usize,
		/// The delta instructions transforming the base into this object.
		delta: Vec<u8>,
	},
}

/// Encode `objects` into a self-contained, delta-compressed packfile (v2) under the
/// hash algorithm `H`.
///
/// Order within `objects` does not matter; the encoder picks its own write order.
pub fn encode_pack<H: HashAlgorithm>(objects: &[PackedObject<H>]) -> Vec<u8> {
	let order = delta_order(objects);
	let plans = plan_deltas(objects, &order);
	write_pack(objects, &order, &plans)
}

/// Choose the write order: group by type, then largest first (a bigger object is a
/// better delta base), then by id for determinism.
fn delta_order<H: HashAlgorithm>(objects: &[PackedObject<H>]) -> Vec<usize> {
	let mut order: Vec<usize> = (0..objects.len()).collect();
	order.sort_by(|&a, &b| {
		let oa = &objects[a];
		let ob = &objects[b];
		type_rank(oa.kind)
			.cmp(&type_rank(ob.kind))
			.then(ob.data.len().cmp(&oa.data.len()))
			.then(oa.id.as_bytes().cmp(ob.id.as_bytes()))
	});
	order
}

/// For each object in write order, try to delta it against a window of earlier
/// objects of the same type, keeping the smallest delta that beats the full form.
fn plan_deltas<H: HashAlgorithm>(objects: &[PackedObject<H>], order: &[usize]) -> Vec<Encoding> {
	let mut plans = Vec::with_capacity(order.len());
	let mut depth = vec![0usize; order.len()];

	for i in 0..order.len() {
		let target = &objects[order[i]];
		let mut best: Option<(usize, Vec<u8>)> = None;
		let lo = i.saturating_sub(WINDOW);
		for j in lo..i {
			let base = &objects[order[j]];
			if base.kind != target.kind || depth[j] >= MAX_DEPTH {
				continue;
			}
			let delta = encode_delta(&base.data, &target.data);
			// Only worth it if smaller than the full payload; prefer the smallest.
			if delta.len() < target.data.len() && best.as_ref().is_none_or(|(_, b)| delta.len() < b.len())
			{
				best = Some((j, delta));
			}
		}
		match best {
			Some((j, delta)) => {
				depth[i] = depth[j] + 1;
				plans.push(Encoding::Ofs { base_pos: j, delta });
			}
			None => plans.push(Encoding::Full),
		}
	}
	plans
}

/// Serialise the planned objects, computing OFS distances and the trailer hash.
fn write_pack<H: HashAlgorithm>(
	objects: &[PackedObject<H>],
	order: &[usize],
	plans: &[Encoding],
) -> Vec<u8> {
	let mut pack = Vec::new();
	pack.extend_from_slice(b"PACK");
	pack.extend_from_slice(&2u32.to_be_bytes());
	pack.extend_from_slice(&(order.len() as u32).to_be_bytes());

	let mut offsets = vec![0usize; order.len()];
	for i in 0..order.len() {
		let entry_start = pack.len();
		offsets[i] = entry_start;
		match &plans[i] {
			Encoding::Full => {
				let object = &objects[order[i]];
				write_obj_header(&mut pack, raw_type(object.kind), object.data.len());
				pack.extend_from_slice(&zlib(&object.data));
			}
			Encoding::Ofs { base_pos, delta } => {
				write_obj_header(&mut pack, OBJ_OFS_DELTA, delta.len());
				write_offset(&mut pack, entry_start - offsets[*base_pos]);
				pack.extend_from_slice(&zlib(delta));
			}
		}
	}

	let trailer = H::digest(&[&pack]);
	pack.extend_from_slice(trailer.as_ref());
	pack
}

/// Encode a delta turning `base` into `target` (gitformat-pack(5) instructions).
///
/// LZ-style greedy matcher: a hash index over `MIN_MATCH`-byte windows of `base`
/// locates candidate runs, which are extended and emitted as copy instructions;
/// unmatched bytes accumulate into insert instructions.
fn encode_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
	let mut out = Vec::new();
	write_varint(&mut out, base.len());
	write_varint(&mut out, target.len());

	let index = index_base(base);
	let mut pending: Vec<u8> = Vec::new();
	let mut i = 0;
	while i < target.len() {
		let (match_off, match_len) = longest_match(base, target, i, &index);
		if match_len >= MIN_MATCH {
			flush_insert(&mut out, &mut pending);
			emit_copy(&mut out, match_off, match_len);
			i += match_len;
		} else {
			pending.push(target[i]);
			if pending.len() == MAX_INSERT {
				flush_insert(&mut out, &mut pending);
			}
			i += 1;
		}
	}
	flush_insert(&mut out, &mut pending);
	out
}

/// Index `base` by `MIN_MATCH`-byte window → the offsets where it occurs.
fn index_base(base: &[u8]) -> HashMap<&[u8], Vec<usize>> {
	let mut index: HashMap<&[u8], Vec<usize>> = HashMap::new();
	if base.len() >= MIN_MATCH {
		for offset in 0..=base.len() - MIN_MATCH {
			index
				.entry(&base[offset..offset + MIN_MATCH])
				.or_default()
				.push(offset);
		}
	}
	index
}

/// Find the longest run in `base` matching `target` starting at `pos`.
fn longest_match(
	base: &[u8],
	target: &[u8],
	pos: usize,
	index: &HashMap<&[u8], Vec<usize>>,
) -> (usize, usize) {
	let mut best_off = 0;
	let mut best_len = 0;
	if pos + MIN_MATCH > target.len() {
		return (best_off, best_len);
	}
	let Some(candidates) = index.get(&target[pos..pos + MIN_MATCH]) else {
		return (best_off, best_len);
	};
	// Most recent offsets first: shorter copy distances compress slightly better.
	for &offset in candidates.iter().rev().take(MAX_CANDIDATES) {
		let mut len = 0;
		while offset + len < base.len()
			&& pos + len < target.len()
			&& base[offset + len] == target[pos + len]
		{
			len += 1;
		}
		if len > best_len {
			best_len = len;
			best_off = offset;
		}
	}
	(best_off, best_len)
}

/// Emit a copy instruction for `len` bytes at `off`, splitting at [`MAX_COPY`].
fn emit_copy(out: &mut Vec<u8>, mut off: usize, mut len: usize) {
	while len > 0 {
		let chunk = len.min(MAX_COPY);
		write_copy(out, off, chunk);
		off += chunk;
		len -= chunk;
	}
}

/// Write one copy instruction: a command byte whose low 4 bits flag which offset
/// bytes follow and whose next 3 bits flag which size bytes follow (little-endian,
/// zero bytes omitted), then those bytes.
fn write_copy(out: &mut Vec<u8>, off: usize, size: usize) {
	let mut cmd = 0x80u8;
	let mut tail = Vec::new();
	for i in 0..4 {
		let byte = (off >> (8 * i) & 0xff) as u8;
		if byte != 0 {
			cmd |= 1 << i;
			tail.push(byte);
		}
	}
	for i in 0..3 {
		let byte = (size >> (8 * i) & 0xff) as u8;
		if byte != 0 {
			cmd |= 0x10 << i;
			tail.push(byte);
		}
	}
	out.push(cmd);
	out.extend_from_slice(&tail);
}

/// Flush accumulated literal bytes as an insert instruction (no-op if empty).
fn flush_insert(out: &mut Vec<u8>, pending: &mut Vec<u8>) {
	if pending.is_empty() {
		return;
	}
	out.push(pending.len() as u8);
	out.append(pending);
}

/// Write a little-endian base-128 varint (the delta source/target size encoding).
fn write_varint(out: &mut Vec<u8>, mut value: usize) {
	loop {
		let mut byte = (value & 0x7f) as u8;
		value >>= 7;
		if value != 0 {
			byte |= 0x80;
		}
		out.push(byte);
		if value == 0 {
			break;
		}
	}
}

/// Write a per-entry `type` + uncompressed `size` header.
fn write_obj_header(out: &mut Vec<u8>, obj_type: u8, mut size: usize) {
	let mut byte = (obj_type << 4) | (size & 0x0f) as u8;
	size >>= 4;
	while size != 0 {
		out.push(byte | 0x80);
		byte = (size & 0x7f) as u8;
		size >>= 7;
	}
	out.push(byte);
}

/// Write an OFS-delta base distance (big-endian base-128, offset-encoded).
fn write_offset(out: &mut Vec<u8>, mut value: usize) {
	let mut bytes = vec![(value & 0x7f) as u8];
	value >>= 7;
	while value != 0 {
		value -= 1;
		bytes.push(0x80 | (value & 0x7f) as u8);
		value >>= 7;
	}
	bytes.reverse();
	out.extend_from_slice(&bytes);
}

/// Compress `data` as a complete zlib stream.
fn zlib(data: &[u8]) -> Vec<u8> {
	let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
	encoder
		.write_all(data)
		.expect("writing to an in-memory zlib encoder is infallible");
	encoder
		.finish()
		.expect("finishing an in-memory zlib encoder is infallible")
}

/// The pack's numeric type code for an object kind.
fn raw_type(kind: ObjectKind) -> u8 {
	match kind {
		ObjectKind::Commit => OBJ_COMMIT,
		ObjectKind::Tree => OBJ_TREE,
		ObjectKind::Blob => OBJ_BLOB,
		ObjectKind::Tag => OBJ_TAG,
	}
}

/// Group order for the write order: commits, tags, trees, then blobs.
fn type_rank(kind: ObjectKind) -> u8 {
	match kind {
		ObjectKind::Commit => 0,
		ObjectKind::Tag => 1,
		ObjectKind::Tree => 2,
		ObjectKind::Blob => 3,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ObjectId, Sha1, Sha256, apply_delta, decode_pack};

	fn blob(data: &[u8]) -> PackedObject<Sha256> {
		PackedObject {
			id: ObjectId::<Sha256>::compute(ObjectKind::Blob, data),
			kind: ObjectKind::Blob,
			data: data.to_vec(),
		}
	}

	#[test]
	fn single_object_round_trips() {
		let object = blob(b"hello world");
		let pack = encode_pack(std::slice::from_ref(&object));
		let decoded = decode_pack::<Sha256>(&pack).expect("decode");
		assert_eq!(decoded, vec![object]);
	}

	#[test]
	fn empty_pack_round_trips() {
		let pack = encode_pack::<Sha256>(&[]);
		assert_eq!(decode_pack::<Sha256>(&pack).expect("decode"), vec![]);
	}

	#[test]
	fn sha1_pack_round_trips() {
		let object = PackedObject::<Sha1> {
			id: ObjectId::<Sha1>::compute(ObjectKind::Blob, b"hello world"),
			kind: ObjectKind::Blob,
			data: b"hello world".to_vec(),
		};
		let pack = encode_pack(std::slice::from_ref(&object));
		let decoded = decode_pack::<Sha1>(&pack).expect("decode");
		assert_eq!(decoded, vec![object]);
	}

	#[test]
	fn similar_objects_round_trip_through_deltas() {
		// Two near-identical large blobs: the encoder should delta one against the
		// other, and decode_pack must reconstruct both byte-for-byte.
		let mut a = b"the quick brown fox jumps over the lazy dog. ".repeat(50);
		let mut b = a.clone();
		b.extend_from_slice(b"and then keeps on running into the night.");
		a.extend_from_slice(b"!!!");

		let objects = vec![blob(&a), blob(&b)];
		let pack = encode_pack(&objects);
		let mut decoded = decode_pack::<Sha256>(&pack).expect("decode");
		decoded.sort_by_key(|o| o.data.len());
		let mut expected = objects.clone();
		expected.sort_by_key(|o| o.data.len());
		assert_eq!(decoded, expected);
	}

	#[test]
	fn delta_is_actually_smaller_than_full() {
		// A large repetitive base and a one-byte edit: the delta must be far smaller
		// than re-encoding the whole target.
		let base = b"abcdefgh".repeat(1000);
		let mut target = base.clone();
		target.push(b'Z');
		let delta = encode_delta(&base, &target);
		assert!(
			delta.len() < target.len() / 10,
			"delta {} not much smaller than target {}",
			delta.len(),
			target.len()
		);
		assert_eq!(apply_delta(&base, &delta).expect("apply"), target);
	}

	#[test]
	fn delta_handles_empty_and_disjoint() {
		// Empty base, and a target sharing nothing with its base: still valid deltas.
		assert_eq!(
			apply_delta(b"", &encode_delta(b"", b"new content")).expect("apply"),
			b"new content"
		);
		let base = b"aaaaaaaaaaaa";
		let target = b"bbbbbbbbbbbb";
		assert_eq!(
			apply_delta(base, &encode_delta(base, target)).expect("apply"),
			target
		);
	}

	#[test]
	fn write_order_is_independent_of_input_order() {
		let objects = vec![blob(b"one"), blob(b"two"), blob(b"three")];
		let mut reversed = objects.clone();
		reversed.reverse();
		// The same set in any order yields the same pack bytes (deterministic order).
		assert_eq!(encode_pack(&objects), encode_pack(&reversed));
	}
}
