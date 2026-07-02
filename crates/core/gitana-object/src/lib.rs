//! Native git object model and codecs.
//!
//! Pure: this crate turns bytes into git objects and back. It has no knowledge of
//! where objects are stored — that is the file-store / object-store layers' job.
//! Content ids are generic over the hash algorithm `H` (see [`HashAlgorithm`] and
//! docs/hlds/storage-layer.md): every object type is parameterised by `H`, and a
//! concrete algorithm ([`Sha1`] / [`Sha256`]) is chosen at the crate boundary. Leaf
//! crate.

mod commit;
mod delta;
mod enumerate;
mod hash_algorithm;
mod id;
mod idx;
mod kind;
mod loose;
mod pack;
mod pack_encode;
mod pktline;
mod sha1;
mod sha256;
mod signature;
mod tag;
mod text;
mod tree;

pub use commit::{Commit, commit_signed_payload, encode_commit, parse_commit};
pub use delta::apply_delta;
pub use enumerate::{enumerate_objects, referenced_ids};
pub use hash_algorithm::{HashAlgorithm, HashKind};
pub use id::ObjectId;
pub use idx::{PackIndex, PackIndexEntry, decode_pack_index, encode_pack_index};
pub use kind::ObjectKind;
pub use loose::{MAX_OBJECT_SIZE, decode_loose, encode_loose, loose_object_path};
pub use pack::{
	PackEntry, PackedObject, decode_object_at, decode_pack, decode_pack_entry,
	decode_pack_with_bases, pack_index_entries, ref_delta_base_ids,
};
pub use pack_encode::encode_pack;
pub use pktline::{
	DELIM_PKT, FLUSH_PKT, MAX_PKT_DATA, PktLine, RESPONSE_END_PKT, parse_pkt, write_delim,
	write_flush, write_pkt,
};
pub use sha1::Sha1;
pub use sha256::Sha256;
pub use signature::{Signature, TzOffset};
pub use tag::{Tag, encode_tag, parse_tag};
pub use tree::{TreeEntry, encode_tree, parse_tree};

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
	/// A packfile's structure was invalid (bad signature, header, or trailer).
	#[error("malformed packfile")]
	MalformedPack,
	/// A pack index (`.idx`) had a bad signature, version, size, duplicate/out-of-order id,
	/// or trailing checksum — either when decoding one or when asked to encode entries that
	/// cannot form a valid index.
	#[error("malformed pack index")]
	MalformedPackIndex,
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
