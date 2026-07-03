//! A sequential [`std::io::Read`] over an open `wasi:filesystem` file descriptor.

use std::io::Read;

use wasip2::filesystem::types::Descriptor;

use crate::io_error;

/// Adapts WASI's positional `descriptor.read` to the sequential [`Read`] the store's
/// streaming path expects, tracking the next offset itself.
pub(crate) struct DescriptorReader {
	file: Descriptor,
	offset: u64,
	eof: bool,
}

impl DescriptorReader {
	pub(crate) fn new(file: Descriptor) -> Self {
		Self {
			file,
			offset: 0,
			eof: false,
		}
	}
}

impl Read for DescriptorReader {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		if self.eof || buf.is_empty() {
			return Ok(0);
		}
		let (chunk, eof) = self
			.file
			.read(buf.len() as u64, self.offset)
			.map_err(io_error)?;
		// The host returns at most the requested length.
		let n = chunk.len();
		buf[..n].copy_from_slice(&chunk);
		self.offset += n as u64;
		self.eof = eof;
		Ok(n)
	}
}
