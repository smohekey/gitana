//! A sequential [`std::io::Write`] over an open `wasi:filesystem` file descriptor.

use std::io::Write;

use wasip2::filesystem::types::Descriptor;

use crate::io_error;

/// Adapts WASI's positional `descriptor.write` to the sequential [`Write`] the store's
/// temp-file publishing expects, tracking the next offset itself.
pub(crate) struct DescriptorWriter {
	file: Descriptor,
	offset: u64,
}

impl DescriptorWriter {
	pub(crate) fn new(file: Descriptor) -> Self {
		Self { file, offset: 0 }
	}
}

impl Write for DescriptorWriter {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		let written = self.file.write(buf, self.offset).map_err(io_error)?;
		self.offset += written;
		Ok(written as usize)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		// WASI 0.2 `descriptor.write` is unbuffered on the guest side; there is nothing
		// to flush. The caller-controlled file-store durability barrier invokes
		// `descriptor.sync-data` after the logical write batch is complete.
		Ok(())
	}
}
