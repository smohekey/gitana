//! pkt-line framing for the git wire protocol.
//!
//! Each pkt-line is a 4-hex-digit big-endian length prefix (the length counts the
//! prefix itself) followed by that many payload bytes. Three lengths are special
//! control packets that carry no payload: `0000` flush, `0001` delim (protocol v2
//! section separator), `0002` response-end. Pure framing — no knowledge of the
//! commands carried inside. See gitprotocol-pack(5) / gitprotocol-common(5).

use crate::ObjectError;

/// The flush packet (`0000`).
pub const FLUSH_PKT: &[u8] = b"0000";
/// The delimiter packet (`0001`).
pub const DELIM_PKT: &[u8] = b"0001";
/// The response-end packet (`0002`).
pub const RESPONSE_END_PKT: &[u8] = b"0002";

/// Largest payload a single data pkt-line may carry: git caps a whole line at 65520
/// bytes, leaving 65516 for the payload after the 4-byte length prefix.
pub const MAX_PKT_DATA: usize = 65516;

/// A decoded pkt-line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PktLine<'a> {
	/// A `0000` flush packet.
	Flush,
	/// A `0001` delimiter packet (protocol v2 section separator).
	Delim,
	/// A `0002` response-end packet (protocol v2 stateless separator).
	ResponseEnd,
	/// A data packet; the slice is the payload with the length prefix stripped.
	Data(&'a [u8]),
}

/// Append a data pkt-line wrapping `data`.
///
/// Returns [`ObjectError::MalformedPktLine`] if `data` exceeds [`MAX_PKT_DATA`];
/// callers streaming large content must chunk it across multiple lines.
pub fn write_pkt(out: &mut Vec<u8>, data: &[u8]) -> Result<(), ObjectError> {
	if data.len() > MAX_PKT_DATA {
		return Err(ObjectError::MalformedPktLine);
	}
	let len = data.len() + 4;
	out.extend_from_slice(hex4(len as u16).as_slice());
	out.extend_from_slice(data);
	Ok(())
}

/// Append a flush packet (`0000`).
pub fn write_flush(out: &mut Vec<u8>) {
	out.extend_from_slice(FLUSH_PKT);
}

/// Append a delimiter packet (`0001`).
pub fn write_delim(out: &mut Vec<u8>) {
	out.extend_from_slice(DELIM_PKT);
}

/// Parse one pkt-line from the front of `input`, returning it and the number of
/// bytes consumed.
///
/// Errors with [`ObjectError::MalformedPktLine`] on a truncated prefix, a
/// non-hex length, the reserved length `0003`, or a data line whose declared
/// length runs past the end of `input`.
pub fn parse_pkt(input: &[u8]) -> Result<(PktLine<'_>, usize), ObjectError> {
	let prefix = input.get(..4).ok_or(ObjectError::MalformedPktLine)?;
	let len = parse_hex4(prefix)? as usize;
	match len {
		0 => Ok((PktLine::Flush, 4)),
		1 => Ok((PktLine::Delim, 4)),
		2 => Ok((PktLine::ResponseEnd, 4)),
		3 => Err(ObjectError::MalformedPktLine),
		_ => {
			let data = input.get(4..len).ok_or(ObjectError::MalformedPktLine)?;
			Ok((PktLine::Data(data), len))
		}
	}
}

/// Render a `u16` as 4 lowercase hex digits.
fn hex4(value: u16) -> [u8; 4] {
	const HEX: &[u8; 16] = b"0123456789abcdef";
	[
		HEX[(value >> 12 & 0xf) as usize],
		HEX[(value >> 8 & 0xf) as usize],
		HEX[(value >> 4 & 0xf) as usize],
		HEX[(value & 0xf) as usize],
	]
}

/// Parse 4 hex digits (upper- or lowercase) into a length.
fn parse_hex4(bytes: &[u8]) -> Result<u16, ObjectError> {
	let mut value = 0u16;
	for &byte in bytes {
		let digit = match byte {
			b'0'..=b'9' => byte - b'0',
			b'a'..=b'f' => byte - b'a' + 10,
			b'A'..=b'F' => byte - b'A' + 10,
			_ => return Err(ObjectError::MalformedPktLine),
		};
		value = value << 4 | u16::from(digit);
	}
	Ok(value)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn data_line_round_trips() {
		let mut out = Vec::new();
		write_pkt(&mut out, b"want abc\n").expect("write");
		assert_eq!(&out[..4], b"000d");
		let (line, consumed) = parse_pkt(&out).expect("parse");
		assert_eq!(line, PktLine::Data(b"want abc\n"));
		assert_eq!(consumed, out.len());
	}

	#[test]
	fn control_packets_round_trip() {
		let mut out = Vec::new();
		write_flush(&mut out);
		write_delim(&mut out);
		out.extend_from_slice(RESPONSE_END_PKT);

		let (a, na) = parse_pkt(&out).expect("flush");
		assert_eq!(a, PktLine::Flush);
		let (b, nb) = parse_pkt(&out[na..]).expect("delim");
		assert_eq!(b, PktLine::Delim);
		let (c, _) = parse_pkt(&out[na + nb..]).expect("response-end");
		assert_eq!(c, PktLine::ResponseEnd);
	}

	#[test]
	fn empty_data_line_is_a_four_byte_line() {
		// "0004" is an empty (zero-payload) data line, distinct from flush "0000".
		let mut out = Vec::new();
		write_pkt(&mut out, b"").expect("write");
		assert_eq!(out, b"0004");
		let (line, consumed) = parse_pkt(&out).expect("parse");
		assert_eq!(line, PktLine::Data(b""));
		assert_eq!(consumed, 4);
	}

	#[test]
	fn rejects_oversized_payload() {
		let mut out = Vec::new();
		let big = vec![0u8; MAX_PKT_DATA + 1];
		assert!(matches!(
			write_pkt(&mut out, &big),
			Err(ObjectError::MalformedPktLine)
		));
	}

	#[test]
	fn rejects_truncated_and_non_hex() {
		assert!(matches!(
			parse_pkt(b"00"),
			Err(ObjectError::MalformedPktLine)
		));
		assert!(matches!(
			parse_pkt(b"zzzz"),
			Err(ObjectError::MalformedPktLine)
		));
		// Declared length 0010 (16 bytes) but only the 4-byte prefix is present.
		assert!(matches!(
			parse_pkt(b"0010"),
			Err(ObjectError::MalformedPktLine)
		));
		// 0003 is reserved.
		assert!(matches!(
			parse_pkt(b"0003"),
			Err(ObjectError::MalformedPktLine)
		));
	}
}
