use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::{HashAlgorithm, ObjectError, ObjectId, ObjectKind};

/// Maximum decompressed object size (1 GiB). Bounds the loose decoder's output so
/// a crafted zlib stream cannot exhaust memory (see docs/hlds/storage-layer.md).
pub const MAX_OBJECT_SIZE: u64 = 1 << 30;

/// Encode an object to its loose, zlib-compressed on-disk form.
///
/// The compressed bytes wrap the canonical `<kind> <size>\0<payload>` whose SHA-256
/// is [`ObjectId::compute`].
pub fn encode_loose(kind: ObjectKind, payload: &[u8]) -> Vec<u8> {
	let header = format!("{} {}\0", kind.as_str(), payload.len());
	let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
	encoder
		.write_all(header.as_bytes())
		.and_then(|()| encoder.write_all(payload))
		.expect("writing to an in-memory zlib encoder cannot fail");
	encoder
		.finish()
		.expect("finishing an in-memory zlib encoder cannot fail")
}

/// Decode a loose, zlib-compressed object back to its kind and payload.
///
/// Rejects streams that decompress beyond [`MAX_OBJECT_SIZE`] and payloads whose
/// length disagrees with the header.
pub fn decode_loose(compressed: &[u8]) -> Result<(ObjectKind, Vec<u8>), ObjectError> {
	decode_loose_with_limit(compressed, MAX_OBJECT_SIZE)
}

/// Decode a loose object while rejecting a payload larger than `max_payload_size`.
///
/// The bound is checked while inflating, before retaining or copying an oversized payload. A
/// small fixed allowance covers the canonical `<kind> <size>\0` header and is not part of the
/// returned payload budget.
pub fn decode_loose_with_limit(
	compressed: &[u8],
	max_payload_size: u64,
) -> Result<(ObjectKind, Vec<u8>), ObjectError> {
	const MAX_HEADER_SIZE: u64 = 64;
	let max_payload_size = max_payload_size.min(MAX_OBJECT_SIZE);
	let mut raw = inflate_capped(compressed, max_payload_size.saturating_add(MAX_HEADER_SIZE))?;

	let nul = raw
		.iter()
		.position(|&byte| byte == 0)
		.ok_or(ObjectError::MalformedHeader)?;
	let (kind_bytes, size_bytes) = split_header(&raw[..nul])?;

	let kind = ObjectKind::from_wire(kind_bytes)?;
	let declared: u64 = std::str::from_utf8(size_bytes)
		.ok()
		.and_then(|s| s.parse().ok())
		.ok_or(ObjectError::MalformedHeader)?;
	if declared > max_payload_size {
		return Err(ObjectError::TooLarge);
	}

	let actual = raw.len().saturating_sub(nul + 1) as u64;
	if actual != declared {
		return Err(ObjectError::LengthMismatch { declared, actual });
	}
	raw.drain(..=nul);
	Ok((kind, raw))
}

/// The repository-relative path of a loose object: `objects/<aa>/<rest>`.
pub fn loose_object_path<H: HashAlgorithm>(id: &ObjectId<H>) -> String {
	let hex = id.to_hex();
	format!("objects/{}/{}", &hex[..2], &hex[2..])
}

fn split_header(header: &[u8]) -> Result<(&[u8], &[u8]), ObjectError> {
	let space = header
		.iter()
		.position(|&byte| byte == b' ')
		.ok_or(ObjectError::MalformedHeader)?;
	Ok((&header[..space], &header[space + 1..]))
}

fn inflate_capped(compressed: &[u8], cap: u64) -> Result<Vec<u8>, ObjectError> {
	let mut decoder = ZlibDecoder::new(compressed);
	let mut out = Vec::new();
	let mut buf = [0u8; 8192];
	loop {
		let n = decoder
			.read(&mut buf)
			.map_err(|error| ObjectError::Zlib(error.to_string()))?;
		if n == 0 {
			break;
		}
		if out.len() as u64 + n as u64 > cap {
			return Err(ObjectError::TooLarge);
		}
		out.extend_from_slice(&buf[..n]);
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::Sha256;

	#[test]
	fn loose_round_trips_all_kinds() {
		for kind in [
			ObjectKind::Blob,
			ObjectKind::Tree,
			ObjectKind::Commit,
			ObjectKind::Tag,
		] {
			let payload = b"some object payload";
			let (decoded_kind, decoded) = decode_loose(&encode_loose(kind, payload)).expect("round trip");
			assert_eq!(decoded_kind, kind);
			assert_eq!(decoded, payload);
		}
	}

	#[test]
	fn loose_path_splits_first_byte() {
		let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, b"x");
		let hex = id.to_hex();
		assert_eq!(
			loose_object_path(&id),
			format!("objects/{}/{}", &hex[..2], &hex[2..])
		);
	}

	#[test]
	fn rejects_length_mismatch() {
		// Hand-build a loose object whose header lies about the size.
		let mut raw = b"blob 99\0".to_vec();
		raw.extend_from_slice(b"short");
		let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
		encoder.write_all(&raw).unwrap();
		let compressed = encoder.finish().unwrap();
		assert!(matches!(
			decode_loose(&compressed),
			Err(ObjectError::LengthMismatch { .. })
		));
	}

	#[test]
	fn bounded_decode_rejects_declared_payload_before_returning_it() {
		let compressed = encode_loose(ObjectKind::Blob, b"oversized");
		assert!(matches!(
			decode_loose_with_limit(&compressed, 4),
			Err(ObjectError::TooLarge)
		));
	}
}
