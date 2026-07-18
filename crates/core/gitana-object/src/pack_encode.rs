//! Writer for git packfiles (v2): the encoder counterpart to [`crate::decode_pack`].
//!
//! Serialises a set of fully materialised objects, delta-compressing with a git-style
//! sliding window (objects sorted by type then size, each tried against a small window
//! of earlier objects of the same type). In-pack deltas are emitted as `OBJ_OFS_DELTA`
//! entries referencing an earlier entry by byte distance. The trailer is the `H` hash
//! of the whole pack. Round-trips through [`crate::decode_pack`] and is accepted by
//! stock `git index-pack` for the matching object format.
//!
//! [`encode_pack`] produces a **self-contained** pack (every delta base is carried in
//! the pack). [`encode_pack_with_bases`] additionally allows deltifying against a pool
//! of *external* base objects the peer is known to already have — emitted as
//! `OBJ_REF_DELTA` entries referencing the base by id and never written into the pack —
//! producing a **thin** pack. A thin pack must be completed against those bases (see
//! [`crate::decode_pack_with_bases`]) before it can be stored for random access.

use std::collections::HashMap;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::pack::PackedObject;
use crate::{HashAlgorithm, ObjectId, ObjectKind};

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

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
enum Encoding<H: HashAlgorithm> {
	/// Full object: its zlib-compressed payload.
	Full,
	/// OFS delta against an earlier emitted entry (by position in the write order).
	Ofs {
		/// Index into the write order of the base entry.
		base_pos: usize,
		/// The delta instructions transforming the base into this object.
		delta: Vec<u8>,
	},
	/// REF delta against an external base the peer already has (not carried in the
	/// pack): the base is referenced by id, making the pack thin.
	Ref {
		/// Id of the external base object.
		base_id: ObjectId<H>,
		/// The delta instructions transforming the base into this object.
		delta: Vec<u8>,
	},
}

/// One candidate delta base in the combined ordering: either an object being written
/// (`Real`, an index into `objects`) or an external base (`Ext`, an index into
/// `external_bases`) that is only referenced, never emitted.
enum Candidate {
	Real(usize),
	Ext(usize),
}

/// Encode `objects` into a self-contained, delta-compressed packfile (v2) under the
/// hash algorithm `H`.
///
/// Order within `objects` does not matter; the encoder picks its own write order.
pub fn encode_pack<H: HashAlgorithm>(objects: &[PackedObject<H>]) -> Vec<u8> {
	encode_pack_with_bases(objects, &[])
}

/// Encode `objects` into a possibly-**thin** packfile, allowing deltas against
/// `external_bases` — objects the peer already has, which are referenced by id
/// (`OBJ_REF_DELTA`) but never written into the pack.
///
/// The external bases participate in the delta-compression window as candidate bases
/// only; they are never emitted and never themselves deltified. With an empty
/// `external_bases` this is byte-identical to [`encode_pack`] (a self-contained pack).
/// A receiver must complete the resulting pack against the same bases (see
/// [`crate::decode_pack_with_bases`]) before storing it for random access.
pub fn encode_pack_with_bases<H: HashAlgorithm>(
	objects: &[PackedObject<H>],
	external_bases: &[PackedObject<H>],
) -> Vec<u8> {
	let combined = combined_order(objects, external_bases);
	let (order, plans) = plan_deltas(objects, external_bases, &combined);
	write_pack(objects, &order, &plans)
}

/// Order all candidate bases — the objects to write plus the external bases — by type,
/// then largest first (a bigger object is a better delta base), then by id for
/// determinism. Reals appear in this same relative order in the pack's write order.
fn combined_order<H: HashAlgorithm>(
	objects: &[PackedObject<H>],
	external_bases: &[PackedObject<H>],
) -> Vec<Candidate> {
	let mut combined: Vec<Candidate> = (0..objects.len())
		.map(Candidate::Real)
		.chain((0..external_bases.len()).map(Candidate::Ext))
		.collect();
	let key = |c: &Candidate| -> &PackedObject<H> {
		match *c {
			Candidate::Real(i) => &objects[i],
			Candidate::Ext(k) => &external_bases[k],
		}
	};
	combined.sort_by(|a, b| {
		let oa = key(a);
		let ob = key(b);
		type_rank(oa.kind)
			.cmp(&type_rank(ob.kind))
			.then(ob.data.len().cmp(&oa.data.len()))
			.then(oa.id.as_bytes().cmp(ob.id.as_bytes()))
	});
	combined
}

/// Plan each object's encoding against a window of earlier same-type candidates (which
/// may be other objects in the pack → OFS, or external bases → REF), keeping the
/// smallest delta that beats the full form. Returns the pack write order (indices into
/// `objects`, a subsequence of the combined order) alongside the per-entry plans.
fn plan_deltas<H: HashAlgorithm>(
	objects: &[PackedObject<H>],
	external_bases: &[PackedObject<H>],
	combined: &[Candidate],
) -> (Vec<usize>, Vec<Encoding<H>>) {
	let payload = |c: &Candidate| -> (&[u8], ObjectKind) {
		match *c {
			Candidate::Real(i) => (&objects[i].data, objects[i].kind),
			Candidate::Ext(k) => (&external_bases[k].data, external_bases[k].kind),
		}
	};
	// Delta-chain depth per combined position (external bases stay 0: the receiver
	// already holds them complete). Emit position per combined position, for reals.
	let mut depth = vec![0usize; combined.len()];
	let mut emit_pos: Vec<Option<usize>> = vec![None; combined.len()];
	let mut order = Vec::new();
	let mut plans = Vec::new();

	for i in 0..combined.len() {
		let Candidate::Real(oi) = combined[i] else {
			continue;
		};
		let target = &objects[oi];
		let mut best: Option<(usize, Vec<u8>)> = None;
		// In-pack (OFS) bases must precede this entry, so reals are only tried backward.
		// External (REF) bases are referenced by id, independent of pack position, so
		// they are also tried in the forward window — otherwise a target that sorts
		// before its slightly-smaller external base (e.g. an append) would never find it.
		let lo = i.saturating_sub(WINDOW);
		let hi = (i + WINDOW + 1).min(combined.len());
		for j in lo..hi {
			if j == i || matches!(combined[j], Candidate::Real(_) if j > i) {
				continue;
			}
			let (base_data, base_kind) = payload(&combined[j]);
			if base_kind != target.kind || depth[j] >= MAX_DEPTH {
				continue;
			}
			let delta = encode_delta(base_data, &target.data);
			// Only worth it if smaller than the full payload; prefer the smallest.
			if delta.len() < target.data.len() && best.as_ref().is_none_or(|(_, b)| delta.len() < b.len())
			{
				best = Some((j, delta));
			}
		}
		let emit = order.len();
		emit_pos[i] = Some(emit);
		match best {
			Some((j, delta)) => {
				depth[i] = depth[j] + 1;
				match combined[j] {
					// A real base precedes this one in the combined order, so it has
					// already been emitted: reference it by its write-order position.
					Candidate::Real(_) => {
						let base_pos = emit_pos[j].expect("a real base earlier in the order is emitted");
						plans.push(Encoding::Ofs { base_pos, delta });
					}
					Candidate::Ext(k) => plans.push(Encoding::Ref {
						base_id: external_bases[k].id,
						delta,
					}),
				}
			}
			None => plans.push(Encoding::Full),
		}
		order.push(oi);
	}
	(order, plans)
}

/// Serialise the planned objects, computing OFS distances and the trailer hash.
fn write_pack<H: HashAlgorithm>(
	objects: &[PackedObject<H>],
	order: &[usize],
	plans: &[Encoding<H>],
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
			Encoding::Ref { base_id, delta } => {
				write_obj_header(&mut pack, OBJ_REF_DELTA, delta.len());
				pack.extend_from_slice(base_id.as_bytes());
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

	#[test]
	fn empty_bases_is_byte_identical_to_self_contained() {
		// The thin encoder with no external bases must produce exactly the same pack as
		// the self-contained encoder — the non-thin path is unchanged.
		let objects = vec![
			blob(b"the quick brown fox"),
			blob(b"the quick brown fox jumps"),
			blob(b"a wholly different payload here"),
		];
		assert_eq!(encode_pack(&objects), encode_pack_with_bases(&objects, &[]));
	}

	#[test]
	fn thin_pack_ref_deltas_against_external_base() {
		// A large base the peer already has; the object we send is a one-byte edit of it.
		// With the base offered externally, the pack must carry the new object as a REF
		// delta and NOT include the base — yet still complete against it.
		let base_data = b"abcdefgh".repeat(2000);
		let base = blob(&base_data);
		let mut edited = base_data.clone();
		edited.push(b'Z');
		let target = blob(&edited);

		let thin = encode_pack_with_bases(std::slice::from_ref(&target), std::slice::from_ref(&base));

		// The base is not carried: a self-contained decode cannot resolve the REF delta.
		assert!(matches!(
			decode_pack::<Sha256>(&thin),
			Err(crate::ObjectError::UnresolvedDeltaBase)
		));

		// Completing against the external base yields the original object, byte-for-byte.
		let mut bases = std::collections::HashMap::new();
		bases.insert(base.id, (base.kind, base.data.clone()));
		let decoded = crate::decode_pack_with_bases::<Sha256>(&thin, &bases).expect("complete thin");
		assert_eq!(decoded, vec![target.clone()]);

		// It really is thin: the pack is far smaller than the object it delivers.
		assert!(
			thin.len() < target.data.len() / 4,
			"thin pack {} not much smaller than the object {}",
			thin.len(),
			target.data.len()
		);
	}

	#[test]
	fn thin_pack_mixes_ofs_and_ref_deltas() {
		// Two objects to send that delta well against an external base and against each
		// other: the encoder should use both a REF delta (external) and an OFS delta
		// (in-pack), and the set must still reconstruct exactly.
		let base_data = b"lorem ipsum dolor sit amet ".repeat(400);
		let base = blob(&base_data);
		let mut a = base_data.clone();
		a.extend_from_slice(b"CONSECTETUR");
		let mut b = a.clone();
		b.extend_from_slice(b"ADIPISCING");
		let obj_a = blob(&a);
		let obj_b = blob(&b);

		let objects = vec![obj_a.clone(), obj_b.clone()];
		let thin = encode_pack_with_bases(&objects, std::slice::from_ref(&base));

		let mut bases = std::collections::HashMap::new();
		bases.insert(base.id, (base.kind, base.data.clone()));
		let mut decoded =
			crate::decode_pack_with_bases::<Sha256>(&thin, &bases).expect("complete thin");
		decoded.sort_by_key(|o| o.data.len());
		let mut expected = objects;
		expected.sort_by_key(|o| o.data.len());
		assert_eq!(decoded, expected);
	}

	#[test]
	fn unused_external_bases_do_not_appear_in_the_pack() {
		// An external base that nothing deltas against must not bloat or corrupt the pack:
		// the output equals the self-contained pack of just the objects.
		let objects = vec![blob(b"hello"), blob(b"world")];
		let unrelated = blob(&b"z".repeat(500));
		let thin = encode_pack_with_bases(&objects, std::slice::from_ref(&unrelated));
		assert_eq!(decode_pack::<Sha256>(&thin).expect("decode"), objects);
	}
}
