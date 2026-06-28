//! Side-band packing: chunk a packfile across side-band-64k pkt-lines on channel 1.
//!
//! git multiplexes the pack stream over "side-band" channels: each data pkt-line in
//! the packfile section starts with a 1-byte channel id — 1 = pack data, 2 = progress,
//! 3 = error. We send pack bytes on channel 1 only.

use gitana_object::{MAX_PKT_DATA, write_pkt};

use crate::GitHttpError;

/// The side-band channel carrying pack data.
const CHANNEL_PACK: u8 = 1;
/// Largest pack slice per line: a full pkt-line payload minus the channel byte.
const MAX_PACK_CHUNK: usize = MAX_PKT_DATA - 1;

/// Append `pack` to `out` as channel-1 side-band pkt-lines (no trailing flush — the
/// caller closes the packfile section).
pub(crate) fn write_sideband_pack(out: &mut Vec<u8>, pack: &[u8]) -> Result<(), GitHttpError> {
	for chunk in pack.chunks(MAX_PACK_CHUNK) {
		let mut line = Vec::with_capacity(chunk.len() + 1);
		line.push(CHANNEL_PACK);
		line.extend_from_slice(chunk);
		write_pkt(out, &line)?;
	}
	Ok(())
}
