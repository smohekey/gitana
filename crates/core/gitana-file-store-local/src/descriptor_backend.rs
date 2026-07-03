//! Wasm backend over an open `wasi:filesystem` directory descriptor.
//!
//! The descriptor is a *capability*: the host mints it (e.g. a wasmtime embedding
//! pushing a directory into its resource table) and passes it across the component
//! boundary — or a command guest takes it from `wasi:filesystem/preopens`; the store
//! can reach exactly that directory tree and nothing else. No path lookup against the
//! WASI preopen table is involved.

use std::io::{Read, Write};

use wasip2::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags};

use crate::{Backend, DescriptorReader, DescriptorWriter};

/// How many bytes to request per positional `descriptor.read` call.
const READ_CHUNK: u64 = 64 * 1024;

/// Wasm backend: every operation goes through a [`Descriptor`] capability, mirroring
/// what [`cap_std::fs::Dir`] provides natively (cap-std itself does not build for
/// wasip2 on stable Rust).
pub(crate) struct DescriptorBackend {
	pub(crate) dir: Descriptor,
}

impl DescriptorBackend {
	/// Open `path` for reading, following symlinks within the sandbox like the native
	/// backend's `Dir::open` does.
	fn open_for_read(&self, path: &str) -> std::io::Result<Descriptor> {
		self
			.dir
			.open_at(
				PathFlags::SYMLINK_FOLLOW,
				path,
				OpenFlags::empty(),
				DescriptorFlags::READ,
			)
			.map_err(io_error)
	}
}

impl Backend for DescriptorBackend {
	fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
		let file = self.open_for_read(path)?;
		read_from(&file, 0, u64::MAX)
	}

	fn read_range(&self, path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
		let file = self.open_for_read(path)?;
		read_from(&file, offset, length)
	}

	fn create_dir_all(&self, path: &str) -> std::io::Result<()> {
		// WASI's `create-directory-at` is single-component (`open-at` resolves whole
		// paths, directory creation does not), so walk the prefixes.
		if path.is_empty() {
			return Ok(());
		}
		let mut prefix = String::with_capacity(path.len());
		for part in path.split('/') {
			if !prefix.is_empty() {
				prefix.push('/');
			}
			prefix.push_str(part);
			match self.dir.create_directory_at(&prefix) {
				Ok(()) | Err(ErrorCode::Exist) => {}
				Err(code) => return Err(io_error(code)),
			}
		}
		Ok(())
	}

	fn create_new(&self, path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>> {
		match self.dir.open_at(
			PathFlags::empty(),
			path,
			OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
			DescriptorFlags::WRITE,
		) {
			Ok(file) => Ok(Some(Box::new(DescriptorWriter::new(file)))),
			Err(ErrorCode::Exist) => Ok(None),
			Err(code) => Err(io_error(code)),
		}
	}

	fn open_read(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>> {
		Ok(Box::new(DescriptorReader::new(self.open_for_read(path)?)))
	}

	fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
		self.dir.rename_at(from, &self.dir, to).map_err(io_error)
	}

	fn remove_file(&self, path: &str) -> std::io::Result<()> {
		self.dir.unlink_file_at(path).map_err(io_error)
	}

	fn exists(&self, path: &str) -> std::io::Result<bool> {
		match self.dir.stat_at(PathFlags::SYMLINK_FOLLOW, path) {
			Ok(_) => Ok(true),
			Err(ErrorCode::NoEntry) => Ok(false),
			Err(code) => Err(io_error(code)),
		}
	}

	fn size(&self, path: &str) -> std::io::Result<u64> {
		let stat = self
			.dir
			.stat_at(PathFlags::SYMLINK_FOLLOW, path)
			.map_err(io_error)?;
		Ok(stat.size)
	}

	fn list_names(&self, dir_rel: &str) -> std::io::Result<Vec<String>> {
		// Keep an opened subdirectory descriptor alive for the whole iteration.
		let opened;
		let dir = if dir_rel.is_empty() {
			&self.dir
		} else {
			match self.dir.open_at(
				PathFlags::SYMLINK_FOLLOW,
				dir_rel,
				OpenFlags::DIRECTORY,
				DescriptorFlags::READ,
			) {
				Ok(subdir) => {
					opened = subdir;
					&opened
				}
				Err(ErrorCode::NoEntry) => return Ok(Vec::new()),
				Err(code) => return Err(io_error(code)),
			}
		};
		let stream = dir.read_directory().map_err(io_error)?;
		let mut names = Vec::new();
		while let Some(entry) = stream.read_directory_entry().map_err(io_error)? {
			names.push(entry.name);
		}
		Ok(names)
	}
}

/// Read `length` bytes (or to end-of-file) starting at `offset`, via positional
/// `descriptor.read` calls in [`READ_CHUNK`]-sized requests.
fn read_from(file: &Descriptor, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
	let mut out = Vec::new();
	let mut offset = offset;
	let mut remaining = length;
	while remaining > 0 {
		let (chunk, eof) = file
			.read(READ_CHUNK.min(remaining), offset)
			.map_err(io_error)?;
		offset += chunk.len() as u64;
		remaining -= chunk.len() as u64;
		let done = eof || chunk.is_empty();
		out.extend_from_slice(&chunk);
		if done {
			break;
		}
	}
	Ok(out)
}

/// Map a WASI filesystem error code onto `std::io::Error`, preserving the kinds the
/// store's semantics depend on (absent → `NotFound`, collision → `AlreadyExists`).
pub(crate) fn io_error(code: ErrorCode) -> std::io::Error {
	let kind = match code {
		ErrorCode::NoEntry => std::io::ErrorKind::NotFound,
		ErrorCode::Exist => std::io::ErrorKind::AlreadyExists,
		ErrorCode::NotDirectory => std::io::ErrorKind::NotADirectory,
		_ => std::io::ErrorKind::Other,
	};
	std::io::Error::new(kind, format!("wasi filesystem error: {code:?}"))
}
