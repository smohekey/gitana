//! Reader for a git multi-pack-index reachability `.bitmap` (`multi-pack-index-<hash>.bitmap`).
//!
//! A MIDX bitmap answers "what objects are reachable from this commit" without walking history: it
//! stores, for a selection of commits, an EWAH bitmap over the MIDX's bitmap object order (see
//! [`crate::pack_order`]) marking every reachable object. Four leading *type* bitmaps mark which
//! positions are commits, trees, blobs, and tags.
//!
//! Layout (version 1): a 12-byte header (`BITM`, `u16` version, `u16` flags, `u32` bitmapped-commit
//! count) then the MIDX's trailing checksum; the four type EWAHs (commits, trees, blobs, tags); then
//! one entry per bitmapped commit — a `u32` object position, a `u8` XOR offset, a `u8` flags byte,
//! and the commit's reachability EWAH. A commit stored with XOR offset `x` is the XOR of its stored
//! bitmap with the (already reconstructed) bitmap `x` entries earlier, a delta git uses to shrink
//! similar bitmaps. Any trailing hash-cache / trailer is not needed for reachability and is ignored.
//!
//! Two coordinate systems meet here, and git uses them asymmetrically. An entry's *object position*
//! is the commit's lexical (id-sorted `OIDL`) index — git's "nth object" — matching
//! [`MultiPackIndex::object_position`]. The *bits* of every EWAH (the type indexes and each
//! reachability bitmap), however, are in the MIDX bitmap object order, so a set bit resolves to an
//! id via [`MultiPackIndex::object_at_bitmap_position`].
//!
//! The optional lookup-table extension (flag `0x10`) rearranges the entry region; it is not yet
//! supported and is rejected rather than mis-parsed.

use std::collections::HashMap;

use crate::{
	EwahBitmap, HashAlgorithm, MultiPackIndex, ObjectError, ObjectId, ObjectKind, decode_ewah,
};

const SIGNATURE: [u8; 4] = *b"BITM";
const VERSION: u16 = 1;
/// Header flag: a lookup table follows (a layout this reader does not yet handle).
const FLAG_LOOKUP_TABLE: u16 = 0x10;
/// Per-commit-entry preamble: `u32` object position, `u8` XOR offset, `u8` flags.
const ENTRY_HEADER_LEN: usize = 6;
/// Smallest possible EWAH stream (bit size, zero word count, RLW position — all `u32`).
const MIN_EWAH_LEN: usize = 12;

/// A parsed MIDX reachability bitmap: the four type indexes and every bitmapped commit's
/// reachability bitmap, resolved through any XOR deltas.
pub struct BitmapIndex {
	commits: EwahBitmap,
	trees: EwahBitmap,
	blobs: EwahBitmap,
	tags: EwahBitmap,
	/// Reconstructed reachability bitmaps in entry order (an XOR delta may reference an earlier one).
	reachability: Vec<EwahBitmap>,
	/// A commit's lexical object position → its index into [`Self::reachability`].
	by_position: HashMap<u32, usize>,
	/// The MIDX checksum this bitmap is bound to (the header's trailing hash).
	midx_checksum: Vec<u8>,
}

impl BitmapIndex {
	/// The type index for `kind`: the positions whose objects are of that kind.
	pub fn type_bitmap(&self, kind: ObjectKind) -> &EwahBitmap {
		match kind {
			ObjectKind::Commit => &self.commits,
			ObjectKind::Tree => &self.trees,
			ObjectKind::Blob => &self.blobs,
			ObjectKind::Tag => &self.tags,
		}
	}

	/// The MIDX checksum the header binds this bitmap to (compare against the MIDX's own trailer to
	/// confirm they belong together).
	pub fn midx_checksum(&self) -> &[u8] {
		&self.midx_checksum
	}

	/// The lexical object positions (see [`MultiPackIndex::object_position`]) of the commits this
	/// index has a reachability bitmap for.
	pub fn bitmapped_commit_positions(&self) -> impl Iterator<Item = u32> + '_ {
		self.by_position.keys().copied()
	}

	/// The reachability bitmap for the commit at lexical `object_position`: the set of all reachable
	/// object positions *in bitmap object order*. `None` if that commit is not bitmapped.
	pub fn commit_reachability(&self, object_position: u32) -> Option<&EwahBitmap> {
		self
			.by_position
			.get(&object_position)
			.map(|&i| &self.reachability[i])
	}

	/// The object ids reachable from the commit at lexical `object_position`, resolving each set bit
	/// through `midx`'s bitmap object order. `None` unless `midx` is the very index this bitmap was
	/// built over (its checksum must match the header's) and the commit is bitmapped; also `None` if
	/// any set bit fails to resolve — the reachability set is returned whole or not at all, never
	/// silently short or against the wrong index.
	pub fn reachable_object_ids<H: HashAlgorithm>(
		&self,
		object_position: u32,
		midx: &MultiPackIndex<H>,
	) -> Option<Vec<ObjectId<H>>> {
		if self.midx_checksum != midx.checksum() {
			return None;
		}
		self
			.commit_reachability(object_position)?
			.set_bits()
			.map(|pos| midx.object_at_bitmap_position(pos as usize).copied())
			.collect()
	}

	/// The object ids reachable from `commit` — the ergonomic query: resolve the commit's lexical
	/// position in `midx`, then its reachability. `None` if `commit` is absent or not bitmapped.
	pub fn reachable_from<H: HashAlgorithm>(
		&self,
		commit: &ObjectId<H>,
		midx: &MultiPackIndex<H>,
	) -> Option<Vec<ObjectId<H>>> {
		let position = midx.object_position(commit)? as u32;
		self.reachable_object_ids(position, midx)
	}
}

/// Parse a version-1 MIDX reachability `.bitmap`. `H` sets the checksum width. Fails with
/// [`ObjectError::MalformedBitmap`] on a bad signature/version, a truncated stream, an XOR offset
/// referencing before the first entry, or the unsupported lookup-table layout.
pub fn decode_midx_bitmap<H: HashAlgorithm>(bytes: &[u8]) -> Result<BitmapIndex, ObjectError> {
	let raw = H::RAW_LEN;
	let header = bytes.get(0..12 + raw).ok_or(ObjectError::MalformedBitmap)?;
	if header[0..4] != SIGNATURE {
		return Err(ObjectError::MalformedBitmap);
	}
	if u16::from_be_bytes([header[4], header[5]]) != VERSION {
		return Err(ObjectError::MalformedBitmap);
	}
	let flags = u16::from_be_bytes([header[6], header[7]]);
	if flags & FLAG_LOOKUP_TABLE != 0 {
		return Err(ObjectError::MalformedBitmap);
	}
	let entry_count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
	let midx_checksum = header[12..12 + raw].to_vec();

	let mut cursor = 12 + raw;
	let take_ewah = |bytes: &[u8], cursor: &mut usize| -> Result<EwahBitmap, ObjectError> {
		let (bitmap, consumed) =
			decode_ewah(bytes.get(*cursor..).ok_or(ObjectError::MalformedBitmap)?)?;
		*cursor += consumed;
		Ok(bitmap)
	};

	let commits = take_ewah(bytes, &mut cursor)?;
	let trees = take_ewah(bytes, &mut cursor)?;
	let blobs = take_ewah(bytes, &mut cursor)?;
	let tags = take_ewah(bytes, &mut cursor)?;

	// Bound the reservation by what the remaining bytes could actually hold (each entry is at least a
	// preamble plus a minimal EWAH), so a corrupt count cannot trigger a huge up-front allocation.
	let capacity =
		entry_count.min(bytes.len().saturating_sub(cursor) / (ENTRY_HEADER_LEN + MIN_EWAH_LEN));
	let mut reachability: Vec<EwahBitmap> = Vec::with_capacity(capacity);
	let mut by_position: HashMap<u32, usize> = HashMap::with_capacity(capacity);
	for i in 0..entry_count {
		let head = bytes
			.get(cursor..cursor + ENTRY_HEADER_LEN)
			.ok_or(ObjectError::MalformedBitmap)?;
		// The commit's lexical object position (git's "nth object"), not a bitmap-order position.
		let position = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
		let xor_offset = head[4] as usize;
		// head[5] is the entry flags byte (reuse hints); reachability does not need it.
		cursor += ENTRY_HEADER_LEN;
		let stored = take_ewah(bytes, &mut cursor)?;

		// A stored bitmap is the XOR of the actual reachability with the reconstructed bitmap
		// `xor_offset` entries earlier; offset 0 means it is stored directly.
		let actual = if xor_offset == 0 {
			stored
		} else {
			let base = i
				.checked_sub(xor_offset)
				.and_then(|j| reachability.get(j))
				.ok_or(ObjectError::MalformedBitmap)?;
			xor(&stored, base)
		};
		by_position.insert(position, i);
		reachability.push(actual);
	}

	Ok(BitmapIndex {
		commits,
		trees,
		blobs,
		tags,
		reachability,
		by_position,
		midx_checksum,
	})
}

/// The word-wise XOR of two bitmaps (the shorter zero-extended).
fn xor(a: &EwahBitmap, b: &EwahBitmap) -> EwahBitmap {
	let (a, b) = (a.words(), b.words());
	let len = a.len().max(b.len());
	let words = (0..len)
		.map(|i| a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0))
		.collect();
	EwahBitmap::from_words(words)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Sha1, encode_ewah};

	/// Assemble a minimal version-1 MIDX bitmap: header + 20-byte checksum, four type EWAHs, then the
	/// given commit entries `(position, xor_offset, stored_bitmap)`.
	fn build(
		checksum: [u8; 20],
		types: [&EwahBitmap; 4],
		entries: &[(u32, u8, EwahBitmap)],
	) -> Vec<u8> {
		let mut out = Vec::new();
		out.extend_from_slice(&SIGNATURE);
		out.extend_from_slice(&VERSION.to_be_bytes());
		out.extend_from_slice(&0u16.to_be_bytes()); // flags
		out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
		out.extend_from_slice(&checksum);
		for t in types {
			out.extend_from_slice(&encode_ewah(t));
		}
		for (pos, xor_offset, bitmap) in entries {
			out.extend_from_slice(&pos.to_be_bytes());
			out.push(*xor_offset);
			out.push(0); // entry flags
			out.extend_from_slice(&encode_ewah(bitmap));
		}
		out
	}

	#[test]
	fn reads_type_indexes_and_direct_commit_bitmaps() {
		let commits = EwahBitmap::from_set_bits([0, 5]);
		let trees = EwahBitmap::from_set_bits([1, 2]);
		let blobs = EwahBitmap::from_set_bits([3, 4]);
		let tags = EwahBitmap::from_set_bits([]);
		let reach = EwahBitmap::from_set_bits([0, 1, 3]);
		let bytes = build(
			[7u8; 20],
			[&commits, &trees, &blobs, &tags],
			&[(5, 0, reach.clone())],
		);

		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode");
		assert_eq!(index.midx_checksum(), &[7u8; 20]);
		assert_eq!(
			index
				.type_bitmap(ObjectKind::Commit)
				.set_bits()
				.collect::<Vec<_>>(),
			[0, 5]
		);
		assert_eq!(
			index
				.type_bitmap(ObjectKind::Blob)
				.set_bits()
				.collect::<Vec<_>>(),
			[3, 4]
		);
		assert_eq!(index.commit_reachability(5), Some(&reach));
		assert!(index.commit_reachability(99).is_none());
		assert_eq!(index.bitmapped_commit_positions().collect::<Vec<_>>(), [5]);
	}

	#[test]
	fn resolves_an_xor_delta_against_an_earlier_entry() {
		let empty = EwahBitmap::from_set_bits([]);
		let base_reach = EwahBitmap::from_set_bits([0, 1, 2]);
		// Entry 1 is stored XOR'd against entry 0: stored = actual XOR base, so actual = {1,2,3}.
		let actual1 = EwahBitmap::from_set_bits([1, 2, 3]);
		let stored1 = xor(&actual1, &base_reach);
		let bytes = build(
			[0u8; 20],
			[&empty, &empty, &empty, &empty],
			&[(10, 0, base_reach.clone()), (11, 1, stored1)],
		);

		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode");
		assert_eq!(index.commit_reachability(10), Some(&base_reach));
		assert_eq!(
			index
				.commit_reachability(11)
				.unwrap()
				.set_bits()
				.collect::<Vec<_>>(),
			[1, 2, 3],
			"XOR delta reconstructs the actual reachability",
		);
	}

	/// Three-object MIDX; `with_ridx` toggles whether it carries a reverse index.
	fn sample_midx(with_ridx: bool) -> crate::MultiPackIndex<Sha1> {
		use crate::{
			ObjectKind, decode_multi_pack_index, encode_multi_pack_index,
			encode_multi_pack_index_with_reverse_index,
		};
		let names = vec!["pack-a.pack".to_owned()];
		let entries: Vec<crate::MidxEntry<Sha1>> = (0..3u8)
			.map(|i| crate::MidxEntry {
				id: crate::ObjectId::<Sha1>::compute(ObjectKind::Blob, &[i]),
				pack_id: 0,
				offset: u64::from(i) * 10,
			})
			.collect();
		let bytes = if with_ridx {
			encode_multi_pack_index_with_reverse_index(&names, &entries, 0).unwrap()
		} else {
			encode_multi_pack_index(&names, &entries).unwrap()
		};
		decode_multi_pack_index::<Sha1>(&bytes).unwrap()
	}

	#[test]
	fn reachability_fails_closed_when_a_position_cannot_resolve() {
		// A MIDX with no reverse index cannot map bitmap positions to ids. Use the MIDX's own checksum
		// so the query passes the binding check and actually exercises the unresolvable path.
		let midx = sample_midx(false);
		assert!(midx.reverse_index().is_none());
		let empty = EwahBitmap::from_set_bits([]);
		let bytes = build(
			midx.checksum().try_into().unwrap(),
			[&empty, &empty, &empty, &empty],
			&[(0, 0, EwahBitmap::from_set_bits([0]))],
		);
		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode");

		assert!(index.commit_reachability(0).is_some());
		assert!(index.reachable_object_ids(0, &midx).is_none());
	}

	#[test]
	fn reachability_rejects_a_mismatched_midx() {
		// A bitmap whose header names a different MIDX must not resolve against this one, even though
		// it has a reverse index and the position is in range.
		let midx = sample_midx(true);
		assert!(midx.reverse_index().is_some());
		let empty = EwahBitmap::from_set_bits([]);
		let bytes = build(
			[0xAB; 20], // not this MIDX's checksum
			[&empty, &empty, &empty, &empty],
			&[(0, 0, EwahBitmap::from_set_bits([0]))],
		);
		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode");
		assert_ne!(index.midx_checksum(), midx.checksum());
		assert!(index.reachable_object_ids(0, &midx).is_none());
	}

	#[test]
	fn rejects_bad_headers_and_out_of_range_xor() {
		let empty = EwahBitmap::from_set_bits([]);
		let types = [&empty, &empty, &empty, &empty];

		let mut bad_sig = build([0u8; 20], types, &[]);
		bad_sig[0] = b'X';
		assert!(matches!(
			decode_midx_bitmap::<Sha1>(&bad_sig),
			Err(ObjectError::MalformedBitmap)
		));

		// A lookup-table flag is rejected as unsupported.
		let mut lookup = build([0u8; 20], types, &[]);
		lookup[6..8].copy_from_slice(&FLAG_LOOKUP_TABLE.to_be_bytes());
		assert!(matches!(
			decode_midx_bitmap::<Sha1>(&lookup),
			Err(ObjectError::MalformedBitmap)
		));

		// The first entry cannot reference an earlier one via XOR.
		let bad_xor = build([0u8; 20], types, &[(0, 1, empty.clone())]);
		assert!(matches!(
			decode_midx_bitmap::<Sha1>(&bad_xor),
			Err(ObjectError::MalformedBitmap)
		));

		// Truncation is rejected.
		assert!(matches!(
			decode_midx_bitmap::<Sha1>(&bad_sig[..10]),
			Err(ObjectError::MalformedBitmap)
		));
	}
}
