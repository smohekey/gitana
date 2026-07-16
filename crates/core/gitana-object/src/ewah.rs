//! Reader/writer for git's EWAH-compressed bitmaps — the word format every git reachability
//! `.bitmap` (pack or multi-pack-index) is built from.
//!
//! EWAH ("Enhanced Word-Aligned Hybrid") compresses a bitmap viewed as a sequence of 64-bit
//! words: runs of *clean* words (all-zero or all-one) are stored as a count, while *dirty* words
//! are stored verbatim as literals. The compressed stream is a sequence of blocks, each a
//! run-length word (RLW) followed by its literal words:
//!
//! ```text
//!   RLW (64 bits, little-end fields):
//!     bit 0        run bit — the value (0/1) of the clean run
//!     bits 1..=32  run length — how many clean words the run covers (32 bits)
//!     bits 33..=63 literal count — how many literal words follow the RLW (31 bits)
//! ```
//!
//! On disk each stream is: a big-endian `u32` bit size (the logical bit count — git records the
//! highest set bit + 1; we pad to a whole word, since we address bits in whole words), a
//! big-endian `u32` count of compressed 64-bit words, those words (big-endian), then a big-endian
//! `u32` giving the word index of the last RLW (git records its running pointer; we read past it).
//! Decoding accepts either form: the uncompressed words must be exactly `ceil(bit size / 64)`.
//!
//! A bit is addressed the way git addresses it: position `p` is `words[p / 64] >> (p % 64) & 1`
//! (word-major, least-significant-bit first). Build a bitmap with [`EwahBitmap::from_set_bits`],
//! serialize it with [`encode_ewah`], and parse one (advancing past it, since `.bitmap` files
//! concatenate several) with [`decode_ewah`].
//!
//! The on-disk bit size is a `u32`, so the format (like git) addresses fewer than 2³² bits — far
//! above any realistic object count (one bit per object).

use crate::ObjectError;

/// Bits in one EWAH word.
const BITS_PER_WORD: u32 = 64;
/// Width of the RLW run-length field (git splits the 64-bit word 1 + 32 + 31).
const RUNNING_LEN_BITS: u32 = 32;
/// Bit offset of the RLW literal-count field (past the run bit and the run length).
const LITERAL_LEN_SHIFT: u32 = 1 + RUNNING_LEN_BITS;
/// Largest clean-word run a single RLW can encode.
const MAX_RUNNING_LEN: u64 = (1 << RUNNING_LEN_BITS) - 1;
/// Largest literal-word count a single RLW can encode.
const MAX_LITERAL_LEN: u64 = (1 << (BITS_PER_WORD - LITERAL_LEN_SHIFT)) - 1;

/// An uncompressed bitmap: bit `p` is `words[p / 64] >> (p % 64) & 1`. Positions past the backing
/// words read as `0`. This is the decompressed view of an EWAH stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EwahBitmap {
	words: Vec<u64>,
}

impl EwahBitmap {
	/// A bitmap over the given 64-bit words (bit `p` is `words[p / 64] >> (p % 64) & 1`).
	pub fn from_words(words: Vec<u64>) -> Self {
		Self { words }
	}

	/// Build a bitmap from the positions to set. Order does not matter; duplicates are harmless.
	pub fn from_set_bits(bits: impl IntoIterator<Item = u32>) -> Self {
		let mut words: Vec<u64> = Vec::new();
		for pos in bits {
			let word = (pos / BITS_PER_WORD) as usize;
			if word >= words.len() {
				words.resize(word + 1, 0);
			}
			words[word] |= 1u64 << (pos % BITS_PER_WORD);
		}
		Self { words }
	}

	/// The backing words.
	pub fn words(&self) -> &[u64] {
		&self.words
	}

	/// Whether bit `pos` is set.
	pub fn get(&self, pos: u32) -> bool {
		let word = (pos / BITS_PER_WORD) as usize;
		self
			.words
			.get(word)
			.is_some_and(|w| (w >> (pos % BITS_PER_WORD)) & 1 == 1)
	}

	/// The number of set bits.
	pub fn count(&self) -> u64 {
		self.words.iter().map(|w| w.count_ones() as u64).sum()
	}

	/// OR `other` into this bitmap in place, word-wise (growing to hold the longer of the two, so the
	/// shorter is effectively zero-extended). Lets a caller union several bitmaps in bitmap-position
	/// space — O(words) per OR regardless of overlap — before resolving the set bits back to ids once.
	pub fn union_in_place(&mut self, other: &EwahBitmap) {
		if other.words.len() > self.words.len() {
			self.words.resize(other.words.len(), 0);
		}
		for (into, from) in self.words.iter_mut().zip(&other.words) {
			*into |= from;
		}
	}

	/// The set-bit positions, ascending.
	pub fn set_bits(&self) -> impl Iterator<Item = u32> + '_ {
		self.words.iter().enumerate().flat_map(|(index, &word)| {
			let base = index as u32 * BITS_PER_WORD;
			(0..BITS_PER_WORD).filter_map(move |bit| ((word >> bit) & 1 == 1).then_some(base + bit))
		})
	}
}

/// Serialize a bitmap as an EWAH stream (see the module docs for the layout).
pub fn encode_ewah(bitmap: &EwahBitmap) -> Vec<u8> {
	let words = &bitmap.words;
	let mut buffer: Vec<u64> = Vec::new();
	// The word index of the last RLW written — git records its running pointer here.
	let mut last_rlw = 0u32;

	let mut i = 0usize;
	while i < words.len() {
		// A block is an optional run of clean words followed by literal (dirty) words.
		let (run_bit, run_len) = clean_run(words, i);
		i += run_len as usize;

		let literal_start = i;
		while i < words.len() && !is_clean(words[i]) && (i - literal_start) as u64 != MAX_LITERAL_LEN {
			i += 1;
		}
		let literal_len = (i - literal_start) as u64;

		let rlw = run_bit | (run_len << 1) | (literal_len << LITERAL_LEN_SHIFT);
		last_rlw = buffer.len() as u32;
		buffer.push(rlw);
		buffer.extend_from_slice(&words[literal_start..i]);
	}

	let bit_size = (words.len() as u64 * BITS_PER_WORD as u64) as u32;
	let mut out = Vec::with_capacity(8 + buffer.len() * 8 + 4);
	out.extend_from_slice(&bit_size.to_be_bytes());
	out.extend_from_slice(&(buffer.len() as u32).to_be_bytes());
	for word in &buffer {
		out.extend_from_slice(&word.to_be_bytes());
	}
	out.extend_from_slice(&last_rlw.to_be_bytes());
	out
}

/// Like [`decode_ewah`], but reject a header `bit_size` exceeding `max_bits` **before** allocating. The
/// `bit_size` is caller-supplied data, and a single clean-run RLW can inflate a few input bytes into a
/// `bit_size / 64`-word buffer (and a [`EwahBitmap::set_bits`] iteration proportional to `bit_size`) — up to
/// ~512 MiB / billions of positions for a `u32::MAX` size. A caller that knows a real upper bound on the
/// bitmap's logical size passes it here so a crafted header cannot force an outsized decode. For example, a
/// split index's delete/replace bitmaps address positions in the *shared* index, so they cannot exceed its
/// entry count.
pub fn decode_ewah_bounded(
	bytes: &[u8],
	max_bits: u64,
) -> Result<(EwahBitmap, usize), ObjectError> {
	let head = bytes.get(0..8).ok_or(ObjectError::MalformedEwah)?;
	let bit_size = u32::from_be_bytes(head[0..4].try_into().unwrap());
	if bit_size as u64 > max_bits {
		return Err(ObjectError::MalformedEwah);
	}
	decode_ewah(bytes)
}

/// Parse one EWAH stream from the front of `bytes`, returning the bitmap and how many bytes it
/// consumed (so a caller can walk the several streams a `.bitmap` file concatenates). Fails with
/// [`ObjectError::MalformedEwah`] on a truncated stream, an RLW that overruns the buffer, or a bit
/// size that disagrees with the words it decompresses to.
pub fn decode_ewah(bytes: &[u8]) -> Result<(EwahBitmap, usize), ObjectError> {
	let head = bytes.get(0..8).ok_or(ObjectError::MalformedEwah)?;
	let bit_size = u32::from_be_bytes(head[0..4].try_into().unwrap());
	let word_count = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;

	let words_end = 8 + word_count * 8;
	let consumed = words_end + 4;
	if bytes.len() < consumed {
		return Err(ObjectError::MalformedEwah);
	}
	let compressed: Vec<u64> = bytes[8..words_end]
		.chunks_exact(8)
		.map(|c| u64::from_be_bytes(c.try_into().unwrap()))
		.collect();

	// git records the logical bit count (highest set bit + 1); the uncompressed words must be
	// exactly enough to hold it (git trims trailing clean-zero words). This bounds decompression so
	// a corrupt RLW (a small `bit_size` but a run length near 2³²) is rejected *before* allocating,
	// rather than resizing to tens of GiB and only then failing the final size check.
	let expected = (bit_size as usize).div_ceil(BITS_PER_WORD as usize);
	let mut out: Vec<u64> = Vec::new();
	let mut p = 0usize;
	while p < compressed.len() {
		let rlw = compressed[p];
		p += 1;
		let fill = if rlw & 1 == 1 { u64::MAX } else { 0 };
		let run_len = ((rlw >> 1) & MAX_RUNNING_LEN) as usize;
		let literal_len = (rlw >> LITERAL_LEN_SHIFT) as usize;

		let grown = out
			.len()
			.checked_add(run_len)
			.and_then(|n| n.checked_add(literal_len));
		if !matches!(grown, Some(n) if n <= expected) {
			return Err(ObjectError::MalformedEwah);
		}

		out.resize(out.len() + run_len, fill);
		let literal_end = p
			.checked_add(literal_len)
			.ok_or(ObjectError::MalformedEwah)?;
		let literals = compressed
			.get(p..literal_end)
			.ok_or(ObjectError::MalformedEwah)?;
		out.extend_from_slice(literals);
		p = literal_end;
	}

	// Every RLW consumed and the words exactly fill `bit_size` (the loop's bound only caps growth).
	if out.len() != expected {
		return Err(ObjectError::MalformedEwah);
	}

	// Drop any bits past `bit_size` in the final partial word: they are outside the logical bitmap
	// (git leaves them zero), so masking keeps corrupt padding from surfacing as real positions.
	let tail = bit_size % BITS_PER_WORD;
	if tail != 0
		&& let Some(last) = out.last_mut()
	{
		*last &= (1u64 << tail) - 1;
	}
	Ok((EwahBitmap { words: out }, consumed))
}

/// Whether a word is a clean run word (all-zero or all-one).
fn is_clean(word: u64) -> bool {
	word == 0 || word == u64::MAX
}

/// Measure the clean-word run at `words[start]`: its bit value and length (both `0` when the word
/// is dirty). Capped at [`MAX_RUNNING_LEN`].
fn clean_run(words: &[u64], start: usize) -> (u64, u64) {
	let first = words[start];
	if !is_clean(first) {
		return (0, 0);
	}
	let mut len = 0u64;
	while start + (len as usize) < words.len()
		&& words[start + len as usize] == first
		&& len != MAX_RUNNING_LEN
	{
		len += 1;
	}
	((first == u64::MAX) as u64, len)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Encode then decode returns the original words, and the decode consumes the whole stream.
	fn round_trip(words: Vec<u64>) {
		let bitmap = EwahBitmap::from_words(words);
		let bytes = encode_ewah(&bitmap);
		let (decoded, consumed) = decode_ewah(&bytes).expect("decode our own stream");
		assert_eq!(decoded, bitmap, "round-trip preserves the words");
		assert_eq!(consumed, bytes.len(), "decode consumes the whole stream");
	}

	#[test]
	fn round_trips_representative_shapes() {
		round_trip(vec![]); // empty
		round_trip(vec![0]); // a lone clean-zero word
		round_trip(vec![u64::MAX]); // a lone clean-one word
		round_trip(vec![0b1011]); // a lone dirty word
		round_trip(vec![0, 0, 0, 0]); // a zero run
		round_trip(vec![u64::MAX; 5]); // a one run
		round_trip(vec![0xDEAD_BEEF, 0xF00D, 0x1234_5678_9ABC_DEF0]); // consecutive literals
		round_trip(vec![0, 0, 0xABC, 0, u64::MAX, u64::MAX, 0x1, 0]); // runs and literals mixed
		round_trip(vec![0xFF, u64::MAX, 0, 0x1, u64::MAX, 0]); // clean words of both values adjacent
	}

	#[test]
	fn set_bits_and_get_agree_with_positions() {
		let positions = [0u32, 1, 63, 64, 65, 130, 4095];
		let bitmap = EwahBitmap::from_set_bits(positions);
		assert_eq!(bitmap.count(), positions.len() as u64);
		assert_eq!(bitmap.set_bits().collect::<Vec<_>>(), positions);
		for pos in positions {
			assert!(bitmap.get(pos), "bit {pos} is set");
		}
		assert!(!bitmap.get(2));
		assert!(!bitmap.get(4096), "past the last word reads as zero");

		// The set survives an encode/decode round-trip.
		let bytes = encode_ewah(&bitmap);
		let (decoded, _) = decode_ewah(&bytes).expect("decode");
		assert_eq!(decoded.set_bits().collect::<Vec<_>>(), positions);
	}

	#[test]
	fn union_in_place_ors_word_wise_and_zero_extends() {
		// A shorter accumulator grows to hold a longer operand; the union is the set-bit union.
		let mut acc = EwahBitmap::from_set_bits([1, 64]);
		acc.union_in_place(&EwahBitmap::from_set_bits([1, 2, 130]));
		assert_eq!(acc.set_bits().collect::<Vec<_>>(), [1, 2, 64, 130]);

		// OR-ing an empty bitmap (no words) leaves the accumulator unchanged.
		acc.union_in_place(&EwahBitmap::default());
		assert_eq!(acc.set_bits().collect::<Vec<_>>(), [1, 2, 64, 130]);

		// OR is commutative in result: starting from the longer side gives the same set.
		let mut other = EwahBitmap::from_set_bits([1, 2, 130]);
		other.union_in_place(&EwahBitmap::from_set_bits([1, 64]));
		assert_eq!(other.set_bits().collect::<Vec<_>>(), [1, 2, 64, 130]);
	}

	#[test]
	fn decode_rejects_a_truncated_stream() {
		let bytes = encode_ewah(&EwahBitmap::from_set_bits([1, 200, 3000]));
		for cut in [0, 4, 7, bytes.len() - 1] {
			assert!(
				matches!(decode_ewah(&bytes[..cut]), Err(ObjectError::MalformedEwah)),
				"a stream cut to {cut} bytes is rejected",
			);
		}
	}

	#[test]
	fn decode_rejects_an_rlw_overrunning_the_buffer() {
		// bit_size 64 (one word), one compressed word claiming 5 literal words that are not there.
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&64u32.to_be_bytes());
		bytes.extend_from_slice(&1u32.to_be_bytes());
		let rlw = 5u64 << LITERAL_LEN_SHIFT;
		bytes.extend_from_slice(&rlw.to_be_bytes());
		bytes.extend_from_slice(&0u32.to_be_bytes());
		assert!(matches!(
			decode_ewah(&bytes),
			Err(ObjectError::MalformedEwah)
		));
	}

	#[test]
	fn decode_masks_bits_past_the_bit_size() {
		// bit_size 3 with a literal word whose bit 63 is set (outside the logical bitmap): the
		// out-of-range bit must not surface as a position.
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&3u32.to_be_bytes());
		bytes.extend_from_slice(&2u32.to_be_bytes());
		let rlw = 1u64 << LITERAL_LEN_SHIFT; // run 0, one literal word follows
		bytes.extend_from_slice(&rlw.to_be_bytes());
		bytes.extend_from_slice(&((1u64 << 63) | 0b101).to_be_bytes());
		bytes.extend_from_slice(&0u32.to_be_bytes());
		let (bitmap, _) = decode_ewah(&bytes).expect("decode");
		assert_eq!(
			bitmap.set_bits().collect::<Vec<_>>(),
			[0, 2],
			"padding bit 63 dropped"
		);
	}

	#[test]
	fn decode_rejects_a_huge_run_without_allocating() {
		// bit_size 64 (one word expected) but an RLW claiming a ~2^32-word run: must be rejected by
		// the up-front bound, not by resizing tens of GiB first.
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&64u32.to_be_bytes());
		bytes.extend_from_slice(&1u32.to_be_bytes());
		let rlw = MAX_RUNNING_LEN << 1; // run bit 0, run length 2^32-1, no literals
		bytes.extend_from_slice(&rlw.to_be_bytes());
		bytes.extend_from_slice(&0u32.to_be_bytes());
		assert!(matches!(
			decode_ewah(&bytes),
			Err(ObjectError::MalformedEwah)
		));
	}

	#[test]
	fn decode_rejects_a_bit_size_that_disagrees_with_the_words() {
		// One clean-zero run of one word (decompresses to one word = 64 bits) but bit_size says 128.
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&128u32.to_be_bytes());
		bytes.extend_from_slice(&1u32.to_be_bytes());
		let rlw = 1u64 << 1; // run bit 0, run length 1, no literals
		bytes.extend_from_slice(&rlw.to_be_bytes());
		bytes.extend_from_slice(&0u32.to_be_bytes());
		assert!(matches!(
			decode_ewah(&bytes),
			Err(ObjectError::MalformedEwah)
		));
	}

	#[test]
	fn decode_bounded_rejects_an_oversized_bit_size_before_allocating() {
		// A tiny payload claiming a `u32::MAX` bit_size (a clean-zero run) would otherwise force a
		// ~512 MiB decode; a caller-supplied bound smaller than the header's bit_size rejects it up front.
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&u32::MAX.to_be_bytes());
		bytes.extend_from_slice(&1u32.to_be_bytes());
		bytes.extend_from_slice(&(u64::MAX << 1).to_be_bytes()); // huge clean-zero run
		bytes.extend_from_slice(&0u32.to_be_bytes());
		assert!(matches!(
			decode_ewah_bounded(&bytes, 1024),
			Err(ObjectError::MalformedEwah)
		));
		// A well-formed bitmap within the bound still decodes: bit_size 64, one clean-zero word.
		let mut ok = Vec::new();
		ok.extend_from_slice(&64u32.to_be_bytes());
		ok.extend_from_slice(&1u32.to_be_bytes());
		ok.extend_from_slice(&(1u64 << 1).to_be_bytes()); // clean-zero run of one word
		ok.extend_from_slice(&0u32.to_be_bytes());
		let (bitmap, _) = decode_ewah_bounded(&ok, 1024).expect("within bound decodes");
		assert_eq!(bitmap.set_bits().count(), 0);
	}
}
