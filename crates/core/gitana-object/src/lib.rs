//! Native git object model and codecs.
//!
//! Pure: this crate turns bytes into git objects and back. It has no knowledge of
//! where objects are stored — that is the file-store / object-store layers' job.
//! Content ids are SHA-256 (see docs/hlds/storage-layer.md); there is no hash-kind
//! type parameter. Leaf crate.

mod commit;
mod delta;
mod enumerate;
mod id;
mod kind;
mod loose;
mod pack;
mod pack_encode;
mod pktline;
mod signature;
mod tag;
mod text;
mod tree;

pub use commit::{Commit, commit_signed_payload, encode_commit, parse_commit};
pub use delta::apply_delta;
pub use enumerate::{enumerate_objects, referenced_ids};
pub use id::ObjectId;
pub use kind::ObjectKind;
pub use loose::{MAX_OBJECT_SIZE, decode_loose, encode_loose, loose_object_path};
pub use pack::{PackedObject, decode_pack, decode_pack_with_bases, ref_delta_base_ids};
pub use pack_encode::encode_pack;
pub use pktline::{
	DELIM_PKT, FLUSH_PKT, MAX_PKT_DATA, PktLine, RESPONSE_END_PKT, parse_pkt, write_delim,
	write_flush, write_pkt,
};
pub use signature::Signature;
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
	/// A hex object id was not 64 lowercase hex characters.
	#[error("invalid object id")]
	InvalidObjectId,
	/// A packfile's structure was invalid (bad signature, header, or trailer).
	#[error("malformed packfile")]
	MalformedPack,
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
