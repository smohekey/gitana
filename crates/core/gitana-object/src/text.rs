//! Shared helpers for reading object payload headers.

use crate::ObjectError;

/// Split a payload into its header block and message at the first blank line.
pub(crate) fn split_message(payload: &[u8]) -> Result<(&[u8], &str), ObjectError> {
	match payload.windows(2).position(|w| w == b"\n\n") {
		Some(i) => Ok((&payload[..i], as_str(&payload[i + 2..])?)),
		None => Ok((payload, "")),
	}
}

/// Decode bytes as UTF-8, mapping failure to [`ObjectError::MalformedHeader`].
pub(crate) fn as_str(bytes: &[u8]) -> Result<&str, ObjectError> {
	std::str::from_utf8(bytes).map_err(|_| ObjectError::MalformedHeader)
}
