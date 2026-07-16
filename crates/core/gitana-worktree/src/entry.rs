use gitana_object::{HashAlgorithm, ObjectId};

/// The `stat(2)` fields git caches per index entry to detect changes without
/// re-hashing. All are stored as 32-bit values, exactly as the index format does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
	/// Inode change time (seconds, nanoseconds).
	pub ctime_sec: u32,
	pub ctime_nsec: u32,
	/// Modification time (seconds, nanoseconds).
	pub mtime_sec: u32,
	pub mtime_nsec: u32,
	/// Device and inode.
	pub dev: u32,
	pub ino: u32,
	/// Owner uid / gid.
	pub uid: u32,
	pub gid: u32,
	/// File size (truncated to 32 bits, as git stores it).
	pub size: u32,
}

/// One entry in the git index: a staged path with its blob id, mode, and stat cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry<H: HashAlgorithm> {
	/// The cached stat data.
	pub stat: Stat,
	/// The git file mode (e.g. `0o100644`, `0o100755`, `0o120000`).
	pub mode: u32,
	/// The staged blob id.
	pub oid: ObjectId<H>,
	/// The merge stage (0 for a normal, non-conflicted entry).
	pub stage: u8,
	/// The assume-valid (`--assume-unchanged`) flag.
	pub assume_valid: bool,
	/// The skip-worktree flag (sparse checkout): git ignores the working tree for this path — an absent file
	/// is not a deletion and a present one is not compared.
	pub skip_worktree: bool,
	/// The repository-relative path (forward slashes).
	pub path: String,
}
