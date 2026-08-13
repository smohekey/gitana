//! Native git object model and codecs.
//!
//! Pure: this crate turns bytes into git objects and back. It has no knowledge of
//! where objects are stored — that is the file-store / object-store layers' job.
//! Content ids are generic over the hash algorithm `H` (see [`HashAlgorithm`] and
//! docs/hlds/storage-layer.md): every object type is parameterised by `H`, and a
//! concrete algorithm ([`Sha1`] / [`Sha256`]) is chosen at the crate boundary. Leaf
//! crate.

mod bitmap;
mod commit;
mod delta;
mod enumerate;
mod ewah;
mod hash_algorithm;
mod id;
mod idx;
mod kind;
mod loose;
mod midx;
mod pack;
mod pack_encode;
mod pktline;
mod revindex;
mod sha1;
mod sha256;
mod signature;
mod tag;
mod text;
mod tree;

pub use bitmap::{
	BitmapIndex, ReachabilityBitmaps, build_reachability_bitmaps, decode_midx_bitmap,
	encode_midx_bitmap,
};
pub use commit::{
	Commit, commit_signature_and_payload, commit_signed_payload, encode_commit, parse_commit,
	validate_commit_structure,
};
pub use delta::apply_delta;
pub use enumerate::{enumerate_objects, referenced_ids};
pub use ewah::{EwahBitmap, decode_ewah, decode_ewah_bounded, encode_ewah};
pub use hash_algorithm::{HashAlgorithm, HashKind};
pub use id::ObjectId;
pub use idx::{PackIndex, PackIndexEntry, decode_pack_index, encode_pack_index};
pub use kind::ObjectKind;
pub use loose::{MAX_OBJECT_SIZE, decode_loose, encode_loose, loose_object_path};
pub use midx::{
	MidxEntry, MultiPackIndex, decode_multi_pack_index, encode_multi_pack_index,
	encode_multi_pack_index_with_reverse_index,
};
pub use pack::{
	PackEntry, PackedObject, decode_object_at, decode_pack, decode_pack_entry,
	decode_pack_with_bases, pack_index_entries, ref_delta_base_ids,
};
pub use pack_encode::{encode_pack, encode_pack_with_bases};
pub use pktline::{
	DELIM_PKT, FLUSH_PKT, MAX_PKT_DATA, PktLine, RESPONSE_END_PKT, parse_pkt, write_delim,
	write_flush, write_pkt,
};
pub use revindex::pack_order;
pub use sha1::Sha1;
pub use sha256::Sha256;
pub use signature::{Signature, TzOffset};
pub use tag::{Tag, encode_tag, parse_tag, tag_signature_and_payload, tag_signed_payload};
pub use tree::{TreeEntry, encode_tree, parse_tree, validate_tree_structure};

/// Errors from decoding or parsing git objects.
#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
	/// The loose-object header was missing, malformed, or had an unknown kind.
	#[error("malformed object header")]
	MalformedHeader,
	/// The declared payload size did not match the actual payload length.
	#[error("object length mismatch: header says {declared}, payload is {actual}")]
	LengthMismatch {
		/// Size declared in the `<kind> <size>\0` header.
		declared: u64,
		/// Actual decoded payload length.
		actual: u64,
	},
	/// The decompressed object exceeded [`MAX_OBJECT_SIZE`] (zlib-bomb guard).
	#[error("object exceeds maximum size of {MAX_OBJECT_SIZE} bytes")]
	TooLarge,
	/// zlib decompression failed.
	#[error("zlib error: {0}")]
	Zlib(String),
	/// A hex object id was not the algorithm's expected length of lowercase hex
	/// characters, or raw id bytes were not the algorithm's raw width.
	#[error("invalid object id")]
	InvalidObjectId,
	/// A tree entry used an empty, reserved, or slash-containing name.
	#[error("invalid tree entry name")]
	InvalidTreeName,
	/// A tree entry used a mode outside git's canonical tree modes.
	#[error("invalid tree entry mode")]
	InvalidTreeMode,
	/// A tree entry referenced the all-zero object id.
	#[error("tree entry references the null object id")]
	NullTreeEntry,
	/// A tree contained the same raw entry name more than once.
	#[error("duplicate tree entry")]
	DuplicateTreeEntry,
	/// A tree's entries were not in git's canonical directory-aware order.
	#[error("tree entries are not canonically sorted")]
	TreeNotSorted,
	/// A commit's required headers were missing, duplicated, malformed, or out of order.
	#[error("invalid commit structure")]
	InvalidCommitStructure,
	/// A commit's author or committer identity was not valid git identity syntax.
	#[error("invalid commit identity")]
	InvalidCommitIdentity,
	/// A packfile's structure was invalid (bad signature, header, or trailer).
	#[error("malformed packfile")]
	MalformedPack,
	/// A pack index (`.idx`) had a bad signature, version, size, duplicate/out-of-order id,
	/// or trailing checksum — either when decoding one or when asked to encode entries that
	/// cannot form a valid index.
	#[error("malformed pack index")]
	MalformedPackIndex,
	/// A multi-pack-index (`multi-pack-index`) had a bad signature, version, hash version, chunk
	/// table, size, out-of-order id, pack reference, or trailing checksum — decoding or encoding.
	#[error("malformed multi-pack-index")]
	MalformedMultiPackIndex,
	/// An EWAH-compressed bitmap stream was truncated, had an RLW overrunning its buffer, or a bit
	/// size disagreeing with the words it decompresses to.
	#[error("malformed EWAH bitmap")]
	MalformedEwah,
	/// A multi-pack-index reachability `.bitmap` had a bad signature/version, a truncated stream, an
	/// XOR offset referencing before the first entry, or an unsupported (lookup-table) layout.
	#[error("malformed reachability bitmap")]
	MalformedBitmap,
	/// A delta's instructions were invalid or referenced outside the base.
	#[error("malformed delta")]
	MalformedDelta,
	/// A REF delta named a base object not present in the pack (thin pack).
	#[error("unresolved delta base")]
	UnresolvedDeltaBase,
	/// A pkt-line had a truncated prefix, a non-hex length, a reserved length, or a
	/// declared length exceeding the available input.
	#[error("malformed pkt-line")]
	MalformedPktLine,
	/// An object reached from a `want` tip was absent from the object source
	/// (a connectivity gap in the requested history).
	#[error("missing object")]
	MissingObject,
}
