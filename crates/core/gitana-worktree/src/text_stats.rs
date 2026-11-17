/// git's `text_stat` (convert.c): the line endings and characters of a file's content, from which
/// git judges whether it's text and which way its line endings would change.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TextStats {
	pub(crate) nul: usize,
	pub(crate) lonecr: usize,
	pub(crate) lonelf: usize,
	pub(crate) crlf: usize,
	printable: usize,
	nonprintable: usize,
}

impl TextStats {
	/// git's `gather_stats`.
	pub(crate) fn of(content: &[u8]) -> TextStats {
		let mut stats = TextStats::default();
		let mut index = 0;
		while index < content.len() {
			let byte = content[index];
			index += 1;
			match byte {
				b'\r' => {
					if content.get(index) == Some(&b'\n') {
						stats.crlf += 1;
						index += 1;
					} else {
						stats.lonecr += 1;
					}
				}
				b'\n' => stats.lonelf += 1,
				127 => stats.nonprintable += 1,
				// Backspace, tab, escape and form feed count as printable.
				0x08 | b'\t' | 0x1b | 0x0c => stats.printable += 1,
				0 => {
					stats.nul += 1;
					stats.nonprintable += 1;
				}
				byte if byte < 32 => stats.nonprintable += 1,
				_ => stats.printable += 1,
			}
		}
		// A trailing EOF character (^Z) isn't counted as unprintable.
		if content.last() == Some(&0x1a) {
			stats.nonprintable -= 1;
		}
		stats
	}

	/// git's `convert_is_binary`: a lone CR, a NUL, or more than one unprintable character in 128.
	pub(crate) fn is_binary(&self) -> bool {
		self.lonecr > 0 || self.nul > 0 || (self.printable >> 7) < self.nonprintable
	}
}
