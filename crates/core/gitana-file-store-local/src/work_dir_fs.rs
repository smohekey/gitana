use std::io;

use crate::{FileKind, Meta};

/// One entry of a directory listing: its name (a single path component) and its kind, from an
/// `lstat` that does not follow symlinks (so a symlinked subdirectory reads as [`FileKind::Symlink`],
/// not [`FileKind::Dir`] — matching how git walks the working tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
	/// The entry's name, a single `/`-free path component.
	pub name: String,
	/// The entry's kind (not following symlinks).
	pub kind: FileKind,
}

/// A working-tree directory as a capability: all access is relative to a confined root (the work
/// tree), with no ambient authority. The native implementation
/// ([`CapWorkDir`](crate::CapWorkDir)) wraps a `cap_std::fs::Dir`; a WASI implementation (a later
/// slice) will wrap a `wasi:filesystem` directory descriptor.
///
/// Paths are work-tree-relative and `/`-separated. Operations are synchronous — the underlying
/// syscalls (cap-std, or positional WASI descriptor calls) are themselves synchronous, and the
/// working-tree code that drives them already interleaves them with `await`ed object-store reads.
///
/// Confinement is structural: the capability resolves each relative path against its own root and
/// refuses to escape it at the syscall boundary, so this trait carries no path-sanitising of its own
/// (the working tree still applies its lexical `validate_path` guard against `..`/`.git`/traversal
/// before calling in).
pub trait WorkDirFs: Send + Sync + 'static {
	/// `lstat` the entry at `path` (not following a final symlink). `Ok(None)` when nothing is there
	/// — both "no such entry" and "a non-directory occupies an ancestor" (`ENOENT`/`ENOTDIR`) fold to
	/// `None`, since either way `path` names nothing.
	fn lstat(&self, path: &str) -> io::Result<Option<Meta>>;

	/// Read the whole contents of the regular file at `path`.
	fn read(&self, path: &str) -> io::Result<Vec<u8>>;

	/// Read the target of the symlink at `path`, as raw bytes (git stores it verbatim as the blob).
	fn read_link(&self, path: &str) -> io::Result<Vec<u8>>;

	/// List the immediate entries of the directory at `path` (`""` = the work-tree root).
	fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>>;

	/// Write `bytes` as the regular file at `path`, replacing whatever plain file is there, and set
	/// the executable bit iff `executable`. Parent directories must already exist. A capability that
	/// cannot represent the executable bit (WASI) silently writes a non-executable file.
	fn write(&self, path: &str, bytes: &[u8], executable: bool) -> io::Result<()>;

	/// Create a symlink at `path` pointing at `target` (raw bytes, as stored in the blob).
	fn symlink(&self, target: &[u8], path: &str) -> io::Result<()>;

	/// Create the single directory `path` (its parent must already exist).
	fn create_dir(&self, path: &str) -> io::Result<()>;

	/// Rename `from` to `to` within the work tree.
	fn rename(&self, from: &str, to: &str) -> io::Result<()>;

	/// Remove the file or symlink at `path`.
	fn remove_file(&self, path: &str) -> io::Result<()>;

	/// Remove the empty directory at `path`.
	fn remove_dir(&self, path: &str) -> io::Result<()>;

	/// Remove the directory at `path` and everything under it.
	fn remove_dir_all(&self, path: &str) -> io::Result<()>;
}
