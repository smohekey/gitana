//! Wasm [`WorkDirFs`] over an open `wasi:filesystem` directory descriptor.
//!
//! The mirror of [`CapWorkDir`](crate::CapWorkDir) for `wasm32-wasip2`: the working tree is a
//! `wasi:filesystem` directory *descriptor* the host mints and passes across the component
//! boundary, and it is the tree's entire authority — every path resolves against it and escapes
//! are refused at the syscall boundary (cap-std cannot build for wasip2 on stable, so a descriptor
//! stands in for the native `Dir`).
//!
//! WASI's `descriptor-stat` carries no permission bits and no inode identity, so [`Meta`] comes
//! back with `mode`/`dev`/`ino`/`uid`/`gid` at `0`: the executable bit collapses to `100644` (as
//! with `core.fileMode=false`) and the stat cache always re-hashes (as with
//! `core.checkStat=minimal`). Symlinks *are* representable (`symlink-at`/`readlink-at`), so blob
//! targets round-trip — though a target escaping the descriptor root fails closed at the host.

use std::io;

use gitana_path::{GitPath, GitPathComponent};
use wasip2::filesystem::types::{
	Descriptor, DescriptorFlags, DescriptorStat, DescriptorType, ErrorCode, OpenFlags, PathFlags,
};

use crate::{DirEntry, FileKind, Meta, WorkDirFs, io_error};

/// How many bytes to request per positional `descriptor.read` call when slurping a file.
const READ_CHUNK: u64 = 64 * 1024;

/// The wasm [`WorkDirFs`]: a working tree confined to one `wasi:filesystem` [`Descriptor`], with no
/// ambient authority. Constructed from a host-granted descriptor at the component's edge.
pub struct DescriptorWorkDir {
	dir: Descriptor,
}

impl DescriptorWorkDir {
	/// Build a working-tree capability over a host-granted directory descriptor.
	pub fn from_descriptor(dir: Descriptor) -> Self {
		Self { dir }
	}

	/// Open `path` for reading, following symlinks within the sandbox (matching the native
	/// capability's `Dir::read`/`Dir::open`).
	fn open_for_read(&self, path: &GitPath) -> io::Result<Descriptor> {
		let path = utf8_path(path)?;
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

impl WorkDirFs for DescriptorWorkDir {
	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()> {
		utf8_path(path).map(drop)
	}

	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>> {
		// The empty path is the work-tree root itself; `stat-at ""` reports it missing, so stat the
		// directory descriptor directly. Otherwise `lstat` with no `symlink-follow` so a final
		// symlink reads as a symlink, matching how git walks the tree.
		let result = if path.is_root() {
			self.dir.stat()
		} else {
			self.dir.stat_at(PathFlags::empty(), utf8_path(path)?)
		};
		match result {
			Ok(stat) => Ok(Some(meta_of(&stat))),
			// Nothing at `path`: either no such entry, or a non-directory occupies an ancestor.
			Err(ErrorCode::NoEntry | ErrorCode::NotDirectory) => Ok(None),
			Err(code) => Err(io_error(code)),
		}
	}

	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		let file = self.open_for_read(path)?;
		let mut out = Vec::new();
		let mut offset = 0u64;
		loop {
			let (chunk, eof) = file.read(READ_CHUNK, offset).map_err(io_error)?;
			offset += chunk.len() as u64;
			let done = eof || chunk.is_empty();
			out.extend_from_slice(&chunk);
			if done {
				break;
			}
		}
		Ok(out)
	}

	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		// WASI hands back the target as a `string`; git stores those verbatim bytes as the blob.
		Ok(
			self
				.dir
				.readlink_at(utf8_path(path)?)
				.map_err(io_error)?
				.into_bytes(),
		)
	}

	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>> {
		// Keep the opened subdirectory descriptor alive for the whole iteration.
		let opened;
		let dir = if path.is_root() {
			&self.dir
		} else {
			opened = self
				.dir
				.open_at(
					PathFlags::SYMLINK_FOLLOW,
					utf8_path(path)?,
					OpenFlags::DIRECTORY,
					DescriptorFlags::READ,
				)
				.map_err(io_error)?;
			&opened
		};
		let stream = dir.read_directory().map_err(io_error)?;
		let mut out = Vec::new();
		while let Some(entry) = stream.read_directory_entry().map_err(io_error)? {
			out.push(DirEntry {
				name: GitPathComponent::from_utf8(entry.name).map_err(invalid_path)?,
				kind: kind_of_type(entry.type_),
			});
		}
		Ok(out)
	}

	fn write(&self, path: &GitPath, bytes: &[u8], _executable: bool) -> io::Result<()> {
		// Truncate-or-create, then rewrite from offset 0. WASI has no chmod, so the executable bit
		// is silently dropped (a regular file is always `100644`) — the documented degradation.
		let file = self
			.dir
			.open_at(
				PathFlags::empty(),
				utf8_path(path)?,
				OpenFlags::CREATE | OpenFlags::TRUNCATE,
				DescriptorFlags::WRITE,
			)
			.map_err(io_error)?;
		let mut offset = 0u64;
		while (offset as usize) < bytes.len() {
			let written = file
				.write(&bytes[offset as usize..], offset)
				.map_err(io_error)?;
			if written == 0 {
				return Err(io::Error::new(
					io::ErrorKind::WriteZero,
					"wasi descriptor write made no progress",
				));
			}
			offset += written;
		}
		Ok(())
	}

	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()> {
		// WASI's `symlink-at` takes the target as a `string`; git symlink blobs are conventionally
		// UTF-8 paths. A non-UTF-8 target cannot be expressed and fails closed.
		let target = std::str::from_utf8(target)
			.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-utf8 symlink target"))?;
		self
			.dir
			.symlink_at(target, utf8_path(path)?)
			.map_err(io_error)
	}

	fn create_dir(&self, path: &GitPath) -> io::Result<()> {
		self
			.dir
			.create_directory_at(utf8_path(path)?)
			.map_err(io_error)
	}

	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()> {
		self
			.dir
			.rename_at(utf8_path(from)?, &self.dir, utf8_path(to)?)
			.map_err(io_error)
	}

	fn remove_file(&self, path: &GitPath) -> io::Result<()> {
		self.dir.unlink_file_at(utf8_path(path)?).map_err(io_error)
	}

	fn remove_dir(&self, path: &GitPath) -> io::Result<()> {
		self
			.dir
			.remove_directory_at(utf8_path(path)?)
			.map_err(io_error)
	}

	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()> {
		// WASI has no recursive remove, so walk the tree depth-first: unlink files and symlinks,
		// recurse into real subdirectories, then remove the now-empty directory itself.
		for entry in self.read_dir(path)? {
			let child = path.join(&entry.name);
			match entry.kind {
				FileKind::Dir => self.remove_dir_all(&child)?,
				_ => self.remove_file(&child)?,
			}
		}
		self.remove_dir(path)
	}
}

fn utf8_path(path: &GitPath) -> io::Result<&str> {
	path.as_utf8().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::Unsupported,
			"WASI filesystem paths must be valid UTF-8",
		)
	})
}

fn invalid_path(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
	io::Error::new(io::ErrorKind::InvalidData, error)
}

/// The capability-neutral [`Meta`] for a WASI `descriptor-stat`. Permission and identity fields are
/// unavailable under WASI and stay `0`; timestamps degrade to `0` when the host does not maintain
/// them.
fn meta_of(stat: &DescriptorStat) -> Meta {
	let mtime = stat
		.data_modification_timestamp
		.map(|t| (t.seconds as i64, t.nanoseconds))
		.unwrap_or((0, 0));
	let ctime = stat
		.status_change_timestamp
		.map(|t| (t.seconds as i64, t.nanoseconds))
		.unwrap_or((0, 0));
	Meta {
		kind: kind_of_type(stat.type_),
		size: stat.size,
		mtime,
		ctime,
		mode: 0,
		dev: 0,
		ino: 0,
		uid: 0,
		gid: 0,
	}
}

/// The git-relevant [`FileKind`] of a WASI `descriptor-type` (from an `lstat` / directory entry,
/// which does not follow symlinks).
fn kind_of_type(ty: DescriptorType) -> FileKind {
	match ty {
		DescriptorType::RegularFile => FileKind::File,
		DescriptorType::Directory => FileKind::Dir,
		DescriptorType::SymbolicLink => FileKind::Symlink,
		_ => FileKind::Other,
	}
}
