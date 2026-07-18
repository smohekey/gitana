use tokio::io::AsyncRead;

/// An owned, type-erased async byte reader (a streaming read, or a write source).
pub type ByteReader = Box<dyn AsyncRead + Send + Unpin>;

/// An opaque version token for a stored path, used for compare-and-set writes.
///
/// Backend-defined (a counter, an etag, a content hash); callers only round-trip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version(pub Vec<u8>);

/// Error returned by file-store operations.
#[derive(Debug, thiserror::Error)]
pub enum FileStoreError {
	/// No bytes exist at the requested path.
	#[error("path not found")]
	NotFound,
	/// A conditional write found a version other than the expected one.
	#[error("version mismatch")]
	VersionMismatch,
	/// A streamed write exceeded the caller's maximum length.
	#[error("value exceeds the maximum length of {limit} bytes")]
	TooLarge {
		/// The cap the write was given.
		limit: u64,
	},
	/// The backend failed for an implementation-specific reason.
	#[error("file store backend error: {0}")]
	Backend(String),
}

/// Convenience result alias for file-store operations.
pub type Result<T> = std::result::Result<T, FileStoreError>;

/// Split a [`FileStore::list_prefix`] argument into a directory (including its
/// trailing `/`, or empty) and a trailing name fragment. Defines the listing
/// semantics shared by every backend.
pub fn split_prefix(prefix: &str) -> (&str, &str) {
	match prefix.rfind('/') {
		Some(i) => (&prefix[..=i], &prefix[i + 1..]),
		None => ("", prefix),
	}
}

/// Outcome of an immutable write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
	/// The value was written.
	Written,
	/// A value already existed at the path; nothing was written.
	AlreadyExists,
}

/// Outcome of a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
	/// The value was deleted.
	Deleted,
	/// No value existed at the path.
	NotFound,
}

/// Path-addressed byte storage for one repository's git files.
///
/// Paths are git-relative (`HEAD`, `refs/heads/main`, `objects/aa/bb...`).
pub trait FileStore: Send + Sync {
	/// Read the bytes stored at `path` within `repo`.
	fn read_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;

	/// Read the bytes at `path` together with their current [`Version`].
	fn read_path_versioned(
		&self,
		path: &str,
	) -> impl Future<Output = Result<(Vec<u8>, Version)>> + Send;

	/// Write `bytes` at `path`, failing (without overwriting) if a value exists.
	fn write_path_if_absent(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<WriteOutcome>> + Send;

	/// Conditionally write `bytes` at `path`, returning the new [`Version`].
	///
	/// `expected == None` requires the path to be absent; `expected == Some(v)`
	/// requires the current version to equal `v`. Otherwise [`FileStoreError::VersionMismatch`].
	fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> impl Future<Output = Result<Version>> + Send;

	/// Atomically replace the value at `path` with `bytes` (creating it if absent), via a
	/// write-to-temp-then-rename so a reader never sees a partial value.
	///
	/// Unlike [`Self::write_path_cas`], there is no version check and no internal `<path>.lock`
	/// file: serialising writers is the caller's responsibility (e.g. holding an external lock).
	/// This lets a caller that itself holds a lock named `<path>.lock` — as the working tree does
	/// for the index — replace `path` without deadlocking against the store's own compare-and-set
	/// lock, which would otherwise contend for that same name.
	fn write_path_replace(&self, path: &str, bytes: &[u8])
	-> impl Future<Output = Result<()>> + Send;

	/// Delete the value at `path`. `expected == Some(v)` requires a version match.
	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> + Send;

	/// Delete the value at `path` unconditionally, taking **no** internal `<path>.lock`.
	///
	/// The delete twin of [`Self::write_path_replace`]: like it, there is no version check and no
	/// `<path>.lock` acquisition, so a caller already holding a `<path>.lock` (e.g. a ref transaction
	/// mid-commit) can remove `path` without deadlocking against the store's own compare-and-set lock.
	/// A caller that needs the version check, or does not hold an external lock, uses
	/// [`Self::delete_path`] instead.
	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> + Send;

	/// Remove the directory at `path` if it is empty.
	///
	/// For pruning directories left behind by ref writes/locks (git removes empty ref directories so a
	/// stale `refs/heads/foo/` cannot block a later `refs/heads/foo`). Errors if `path` is not an empty
	/// directory (a non-empty directory, a value, or absent), which a best-effort pruner treats as
	/// "stop here". A backend without a directory concept always errors.
	fn remove_dir(&self, path: &str) -> impl Future<Output = Result<()>> + Send;

	/// Whether a value exists at `path` within `repo`.
	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> + Send;

	/// Whether `path` is a directory (as opposed to a value or absent).
	///
	/// Lets a caller preflight a directory/file conflict before writing a value at `path` — e.g. a ref
	/// transaction checking that `refs/heads/foo` (or its reflog `logs/refs/heads/foo`) is not a
	/// directory left by a nested ref. A backend without a directory concept (an in-memory map) always
	/// returns `false`.
	fn is_dir(&self, path: &str) -> impl Future<Output = Result<bool>> + Send;

	/// The byte length of the value at `path`. [`FileStoreError::NotFound`] if it is absent.
	fn size(&self, path: &str) -> impl Future<Output = Result<u64>> + Send;

	/// List repository-relative paths within `repo` that begin with `prefix`.
	///
	/// `prefix` is treated as a directory boundary at its last `/`: the directory
	/// it names is listed and entries are filtered by the trailing name fragment.
	/// Not recursive — sufficient for flat collections like `objects/pack/`.
	fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send;

	/// Read `length` bytes starting at `offset` within the value at `path`.
	///
	/// Reads past the end of the value return fewer bytes; an `offset` past the end
	/// returns empty.
	fn read_path_range(
		&self,
		path: &str,
		offset: u64,
		length: u64,
	) -> impl Future<Output = Result<Vec<u8>>> + Send;

	/// Open the value at `path` as a streaming reader (no whole-value buffering).
	fn read_path_stream(&self, path: &str) -> impl Future<Output = Result<ByteReader>> + Send;

	/// Stream `reader` into `path` if absent, failing once more than `max_len`
	/// bytes have been read ([`FileStoreError::TooLarge`]). The value is never
	/// buffered whole in memory by the backend.
	fn write_path_stream_if_absent(
		&self,
		path: &str,
		reader: ByteReader,
		max_len: u64,
	) -> impl Future<Output = Result<WriteOutcome>> + Send;
}
