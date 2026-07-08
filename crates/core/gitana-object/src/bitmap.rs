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

use std::collections::{HashMap, HashSet};

use crate::{
	EwahBitmap, HashAlgorithm, MultiPackIndex, ObjectError, ObjectId, ObjectKind, decode_ewah,
	encode_ewah,
};

const SIGNATURE: [u8; 4] = *b"BITM";
const VERSION: u16 = 1;
/// Header flag: the bitmaps cover the full closure of each commit (git sets this; we always do).
const FLAG_FULL_DAG: u16 = 0x1;
/// Header flag: a lookup table follows (a layout this reader does not yet handle).
const FLAG_LOOKUP_TABLE: u16 = 0x10;
/// Per-commit-entry preamble: `u32` object position, `u8` XOR offset, `u8` flags.
const ENTRY_HEADER_LEN: usize = 6;
/// Smallest possible EWAH stream (bit size, zero word count, RLW position — all `u32`).
const MIN_EWAH_LEN: usize = 12;
/// Tree-entry mode for a submodule gitlink (a commit id in another repository).
const GITLINK_MODE: &str = "160000";

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

/// Serialize a version-1 MIDX reachability `.bitmap` for the MIDX identified by `midx_checksum`
/// (which must be `H::RAW_LEN` bytes). `type_bitmaps` are the four type indexes in git's order —
/// commits, trees, blobs, tags — over the MIDX bitmap object order; `commits` pairs each bitmapped
/// commit's lexical object position (see [`MultiPackIndex::object_position`]) with its reachability
/// bitmap in that same order.
///
/// The output is the minimal form stock git accepts: `FLAG_FULL_DAG` set, every commit stored
/// directly (XOR offset 0), no hash-cache, and an `H` trailer over the preceding bytes. It is the
/// inverse of [`decode_midx_bitmap`]. The caller writes it to `multi-pack-index-<checksum>.bitmap`.
///
/// Fails with [`ObjectError::MalformedBitmap`] if `midx_checksum` is not `H::RAW_LEN` bytes — a
/// wrong-width checksum would shift the first EWAH off the fixed offset git and the reader expect.
pub fn encode_midx_bitmap<H: HashAlgorithm>(
	midx_checksum: &[u8],
	type_bitmaps: [&EwahBitmap; 4],
	commits: &[(u32, &EwahBitmap)],
) -> Result<Vec<u8>, ObjectError> {
	if midx_checksum.len() != H::RAW_LEN {
		return Err(ObjectError::MalformedBitmap);
	}
	let mut out = Vec::new();
	out.extend_from_slice(&SIGNATURE);
	out.extend_from_slice(&VERSION.to_be_bytes());
	out.extend_from_slice(&FLAG_FULL_DAG.to_be_bytes());
	out.extend_from_slice(&(commits.len() as u32).to_be_bytes());
	out.extend_from_slice(midx_checksum);

	for type_bitmap in type_bitmaps {
		out.extend_from_slice(&encode_ewah(type_bitmap));
	}
	for (object_position, reachable) in commits {
		out.extend_from_slice(&object_position.to_be_bytes());
		out.push(0); // XOR offset: stored directly
		out.push(0); // entry flags
		out.extend_from_slice(&encode_ewah(reachable));
	}

	let checksum = H::digest(&[&out]);
	out.extend_from_slice(checksum.as_ref());
	Ok(out)
}

/// The bitmaps a reachability `.bitmap` is built from: the four type indexes (which positions hold
/// commits, trees, blobs, tags) and each selected commit's reachability bitmap, all in the MIDX
/// bitmap object order. Produced by [`build_reachability_bitmaps`], consumed by
/// [`encode_midx_bitmap`] (directly, or via [`Self::encode`]).
pub struct ReachabilityBitmaps {
	commit_type: EwahBitmap,
	tree_type: EwahBitmap,
	blob_type: EwahBitmap,
	tag_type: EwahBitmap,
	commits: Vec<(u32, EwahBitmap)>,
}

impl ReachabilityBitmaps {
	/// The four type indexes in git's order — commits, trees, blobs, tags.
	pub fn type_bitmaps(&self) -> [&EwahBitmap; 4] {
		[
			&self.commit_type,
			&self.tree_type,
			&self.blob_type,
			&self.tag_type,
		]
	}

	/// The bitmapped commits: each commit's lexical object position and reachability bitmap.
	pub fn commits(&self) -> Vec<(u32, &EwahBitmap)> {
		self.commits.iter().map(|(pos, bm)| (*pos, bm)).collect()
	}

	/// Serialize these into a `.bitmap` for the MIDX identified by `midx_checksum` (see
	/// [`encode_midx_bitmap`]).
	pub fn encode<H: HashAlgorithm>(&self, midx_checksum: &[u8]) -> Result<Vec<u8>, ObjectError> {
		encode_midx_bitmap::<H>(midx_checksum, self.type_bitmaps(), &self.commits())
	}
}

/// Build the reachability and type bitmaps for a MIDX (which must carry a reverse index) over the
/// given `selected_commits`. `kind_of` gives every object's kind — for the type indexes and to keep
/// the walk from reading blob bodies (blobs are leaves); `read_object` returns a commit/tree/tag's
/// bytes so the walk can follow its children. Neither is ever called on a blob for its payload.
///
/// Each commit's reachability is its full object closure, walked independently — so keep
/// `selected_commits` sparse (git bitmaps ref tips plus a sampling, not every commit). Fails with
/// [`ObjectError::MissingObject`] if a reachable object is absent from the MIDX or the readers, and
/// [`ObjectError::MalformedMultiPackIndex`] if the MIDX has no reverse index.
pub fn build_reachability_bitmaps<H, K, R>(
	midx: &MultiPackIndex<H>,
	selected_commits: &[ObjectId<H>],
	kind_of: K,
	read_object: R,
) -> Result<ReachabilityBitmaps, ObjectError>
where
	H: HashAlgorithm,
	K: Fn(&ObjectId<H>) -> Option<ObjectKind>,
	R: Fn(&ObjectId<H>) -> Option<Vec<u8>>,
{
	let reverse = midx
		.reverse_index()
		.ok_or(ObjectError::MalformedMultiPackIndex)?;

	// The bitmap position of each object by its lexical index (the inverse of the reverse index).
	let mut position_by_lexical = vec![0u32; reverse.len()];
	for (bitmap_pos, &lexical) in reverse.iter().enumerate() {
		position_by_lexical[lexical as usize] = bitmap_pos as u32;
	}
	let position_of = |id: &ObjectId<H>| -> Option<u32> {
		midx.object_position(id).map(|lex| position_by_lexical[lex])
	};

	// Type indexes: every object's position, bucketed by kind.
	let mut by_kind: [Vec<u32>; 4] = [const { Vec::new() }; 4];
	for (lexical, id) in midx.object_ids().iter().enumerate() {
		let kind = kind_of(id).ok_or(ObjectError::MissingObject)?;
		by_kind[type_slot(kind)].push(position_by_lexical[lexical]);
	}
	let [commit_type, tree_type, blob_type, tag_type] = by_kind.map(EwahBitmap::from_set_bits);

	// Each selected commit's reachability: an independent closure walk that never reads a blob body.
	let mut commits = Vec::with_capacity(selected_commits.len());
	for commit in selected_commits {
		let entry_position = midx
			.object_position(commit)
			.ok_or(ObjectError::MissingObject)? as u32;
		let mut visited: HashSet<ObjectId<H>> = HashSet::new();
		let mut positions: Vec<u32> = Vec::new();
		let mut stack: Vec<ObjectId<H>> = vec![*commit];
		while let Some(id) = stack.pop() {
			if !visited.insert(id) {
				continue;
			}
			positions.push(position_of(&id).ok_or(ObjectError::MissingObject)?);
			match kind_of(&id).ok_or(ObjectError::MissingObject)? {
				// Blobs are leaves; never read their bodies.
				ObjectKind::Blob => {}
				ObjectKind::Tree => {
					let data = read_object(&id).ok_or(ObjectError::MissingObject)?;
					for entry in crate::parse_tree::<H>(&data)? {
						// A submodule gitlink points at a commit in another repository — not an object
						// here, and not in this MIDX — so it is not part of this repo's reachability.
						if entry.mode != GITLINK_MODE {
							stack.push(entry.id);
						}
					}
				}
				// A commit's tree and parents, and a tag's target, are always real objects here.
				kind => {
					let data = read_object(&id).ok_or(ObjectError::MissingObject)?;
					stack.extend(crate::referenced_ids::<H>(kind, &data)?);
				}
			}
		}
		commits.push((entry_position, EwahBitmap::from_set_bits(positions)));
	}

	Ok(ReachabilityBitmaps {
		commit_type,
		tree_type,
		blob_type,
		tag_type,
		commits,
	})
}

/// The type-index slot for `kind`, in git's order (commits, trees, blobs, tags).
fn type_slot(kind: ObjectKind) -> usize {
	match kind {
		ObjectKind::Commit => 0,
		ObjectKind::Tree => 1,
		ObjectKind::Blob => 2,
		ObjectKind::Tag => 3,
	}
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
	fn builds_reachability_and_type_bitmaps() {
		use std::collections::HashMap;

		use crate::{
			Commit, MidxEntry, TreeEntry, decode_multi_pack_index, encode_commit,
			encode_multi_pack_index_with_reverse_index, encode_tree,
		};

		// A tiny graph: blob <- tree <- commit c1 <- commit c2 (c2 reuses c1's tree).
		let mut store: HashMap<ObjectId<Sha1>, (ObjectKind, Vec<u8>)> = HashMap::new();
		let mut put = |kind: ObjectKind, data: Vec<u8>| {
			let id = ObjectId::<Sha1>::compute(kind, &data);
			store.insert(id, (kind, data));
			id
		};
		let blob = put(ObjectKind::Blob, b"hello\n".to_vec());
		let tree = put(
			ObjectKind::Tree,
			encode_tree(&[TreeEntry {
				mode: "100644".to_owned(),
				name: "f".to_owned(),
				id: blob,
			}]),
		);
		let sig = "A U Thor <a@x> 1700000000 +0000".to_owned();
		let c1 = put(
			ObjectKind::Commit,
			encode_commit(&Commit {
				tree,
				parents: vec![],
				author: sig.clone(),
				committer: sig.clone(),
				signature: None,
				extra_headers: Vec::new(),
				message: "one\n".to_owned(),
			}),
		);
		let c2 = put(
			ObjectKind::Commit,
			encode_commit(&Commit {
				tree,
				parents: vec![c1],
				author: sig.clone(),
				committer: sig,
				signature: None,
				extra_headers: Vec::new(),
				message: "two\n".to_owned(),
			}),
		);

		let names = vec!["pack-a.pack".to_owned()];
		let entries: Vec<MidxEntry<Sha1>> = [blob, tree, c1, c2]
			.iter()
			.enumerate()
			.map(|(i, &id)| MidxEntry {
				id,
				pack_id: 0,
				offset: (i as u64 + 1) * 100,
			})
			.collect();
		let midx = decode_multi_pack_index::<Sha1>(
			&encode_multi_pack_index_with_reverse_index(&names, &entries, 0).unwrap(),
		)
		.unwrap();

		let built = build_reachability_bitmaps(
			&midx,
			&[c1, c2],
			|id| store.get(id).map(|(k, _)| *k),
			|id| store.get(id).map(|(_, d)| d.clone()),
		)
		.expect("build");

		// Type indexes bucket every object by kind (mapped back through the bitmap order).
		let ids_of = |bm: &EwahBitmap| -> std::collections::HashSet<ObjectId<Sha1>> {
			bm.set_bits()
				.map(|p| *midx.object_at_bitmap_position(p as usize).unwrap())
				.collect()
		};
		let [commits_t, trees_t, blobs_t, tags_t] = built.type_bitmaps();
		assert_eq!(ids_of(commits_t), HashSet::from([c1, c2]));
		assert_eq!(ids_of(trees_t), HashSet::from([tree]));
		assert_eq!(ids_of(blobs_t), HashSet::from([blob]));
		assert!(tags_t.set_bits().next().is_none());

		// Reachability: c1 reaches {c1, tree, blob}; c2 also pulls in c1.
		let reach = |commit: ObjectId<Sha1>| -> HashSet<ObjectId<Sha1>> {
			let pos = midx.object_position(&commit).unwrap() as u32;
			let (_, bm) = built
				.commits()
				.into_iter()
				.find(|(p, _)| *p == pos)
				.unwrap();
			ids_of(bm)
		};
		assert_eq!(reach(c1), HashSet::from([c1, tree, blob]));
		assert_eq!(reach(c2), HashSet::from([c2, c1, tree, blob]));

		// The whole pipeline round-trips through the writer + reader.
		let bytes = built.encode::<Sha1>(midx.checksum()).expect("encode");
		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode");
		let via_reader: HashSet<ObjectId<Sha1>> = index
			.reachable_from(&c2, &midx)
			.unwrap()
			.into_iter()
			.collect();
		assert_eq!(via_reader, HashSet::from([c2, c1, tree, blob]));
	}

	#[test]
	fn skips_submodule_gitlinks() {
		use std::collections::HashMap;

		use crate::{
			Commit, MidxEntry, TreeEntry, decode_multi_pack_index, encode_commit,
			encode_multi_pack_index_with_reverse_index, encode_tree,
		};

		let mut store: HashMap<ObjectId<Sha1>, (ObjectKind, Vec<u8>)> = HashMap::new();
		let mut put = |kind: ObjectKind, data: Vec<u8>| {
			let id = ObjectId::<Sha1>::compute(kind, &data);
			store.insert(id, (kind, data));
			id
		};
		let blob = put(ObjectKind::Blob, b"x\n".to_vec());
		// A gitlink id that is deliberately absent from the store and the MIDX.
		let gitlink = ObjectId::<Sha1>::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
		let tree = put(
			ObjectKind::Tree,
			encode_tree(&[
				TreeEntry {
					mode: "100644".to_owned(),
					name: "f".to_owned(),
					id: blob,
				},
				TreeEntry {
					mode: "160000".to_owned(),
					name: "sub".to_owned(),
					id: gitlink,
				},
			]),
		);
		let sig = "A U Thor <a@x> 1700000000 +0000".to_owned();
		let commit = put(
			ObjectKind::Commit,
			encode_commit(&Commit {
				tree,
				parents: vec![],
				author: sig.clone(),
				committer: sig,
				signature: None,
				extra_headers: Vec::new(),
				message: "sub\n".to_owned(),
			}),
		);

		let names = vec!["pack-a.pack".to_owned()];
		let entries: Vec<MidxEntry<Sha1>> = [blob, tree, commit]
			.iter()
			.enumerate()
			.map(|(i, &id)| MidxEntry {
				id,
				pack_id: 0,
				offset: (i as u64 + 1) * 100,
			})
			.collect();
		let midx = decode_multi_pack_index::<Sha1>(
			&encode_multi_pack_index_with_reverse_index(&names, &entries, 0).unwrap(),
		)
		.unwrap();

		// The gitlink is neither read nor required: the build succeeds and omits it.
		let built = build_reachability_bitmaps(
			&midx,
			&[commit],
			|id| store.get(id).map(|(k, _)| *k),
			|id| store.get(id).map(|(_, d)| d.clone()),
		)
		.expect("build ignores the gitlink");
		let (_, bm) = built.commits().into_iter().next().unwrap();
		let reached: std::collections::HashSet<ObjectId<Sha1>> = bm
			.set_bits()
			.map(|p| *midx.object_at_bitmap_position(p as usize).unwrap())
			.collect();
		assert_eq!(reached, HashSet::from([commit, tree, blob]));
		assert!(!reached.contains(&gitlink));
	}

	#[test]
	fn encode_then_decode_round_trips() {
		let commits_t = EwahBitmap::from_set_bits([0, 5]);
		let trees_t = EwahBitmap::from_set_bits([1, 2]);
		let blobs_t = EwahBitmap::from_set_bits([3, 4]);
		let tags_t = EwahBitmap::from_set_bits([]);
		let reach0 = EwahBitmap::from_set_bits([0, 1, 3]);
		let reach5 = EwahBitmap::from_set_bits([0, 1, 2, 3, 4, 5]);

		let types = [&commits_t, &trees_t, &blobs_t, &tags_t];
		let entries = [(0u32, &reach0), (5, &reach5)];
		let bytes = encode_midx_bitmap::<Sha1>(&[9u8; 20], types, &entries).expect("encode");
		// A wrong-width checksum is rejected rather than written into a malformed header.
		assert!(matches!(
			encode_midx_bitmap::<Sha1>(&[9u8; 19], types, &entries),
			Err(ObjectError::MalformedBitmap)
		));
		// The trailer is an H digest over everything before it.
		assert_eq!(
			&bytes[bytes.len() - 20..],
			crate::Sha1::digest(&[&bytes[..bytes.len() - 20]]).as_ref(),
		);

		let index = decode_midx_bitmap::<Sha1>(&bytes).expect("decode our own bitmap");
		assert_eq!(index.midx_checksum(), &[9u8; 20]);
		assert_eq!(
			index
				.type_bitmap(ObjectKind::Commit)
				.set_bits()
				.collect::<Vec<_>>(),
			[0, 5]
		);
		assert_eq!(index.commit_reachability(0), Some(&reach0));
		assert_eq!(index.commit_reachability(5), Some(&reach5));
		let mut positions: Vec<u32> = index.bitmapped_commit_positions().collect();
		positions.sort_unstable();
		assert_eq!(positions, [0, 5]);
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
