use crate::FileKind;

/// Capability-neutral metadata for a working-tree entry — the `lstat` fields the working tree and
/// index need, reported by whatever [`WorkDirFs`](crate::WorkDirFs) backs the tree.
///
/// A native (cap-std) capability fills every field from the real `stat(2)`. A WASI descriptor
/// capability fills only what `wasi:filesystem`'s `descriptor-stat` provides — kind, size, and the
/// timestamps — and leaves `mode`/`dev`/`ino`/`uid`/`gid` at `0`, which git tolerates: the missing
/// permission bit collapses the executable mode to `100644` (as with `core.fileMode=false`) and the
/// missing stat-cache identity forces `status`/`diff` to re-hash rather than trust the cache (as with
/// `core.checkStat=minimal`). Deriving git's mode and the index stat cache from this struct — rather
/// than from a compile-time `cfg(unix)` split — lets a native capability keep full fidelity while a
/// wasm one degrades, following the capability instead of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
	/// The entry's kind (from an `lstat` that does not follow symlinks).
	pub kind: FileKind,
	/// Size in bytes.
	pub size: u64,
	/// Last-modification time, as `(seconds, nanoseconds)` since the Unix epoch.
	pub mtime: (i64, u32),
	/// Last status-change time, as `(seconds, nanoseconds)` since the Unix epoch.
	pub ctime: (i64, u32),
	/// The unix mode bits (type + permissions), or `0` if the capability cannot report them. Only
	/// the executable bits (`& 0o111`) are consulted, to pick git's `100755` vs `100644`.
	pub mode: u32,
	/// Device id, or `0` if unavailable.
	pub dev: u64,
	/// Inode number, or `0` if unavailable.
	pub ino: u64,
	/// Owner uid, or `0` if unavailable.
	pub uid: u32,
	/// Owner gid, or `0` if unavailable.
	pub gid: u32,
}
