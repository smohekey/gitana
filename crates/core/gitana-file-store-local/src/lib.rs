//! Filesystem [`FileStore`] backend for local / standalone deployments.
//!
//! Stores git-relative paths beneath a confined root. The raw filesystem primitives
//! live behind an internal `Backend`; the store's semantics (atomic temp+rename,
//! content-hash CAS, per-path locking, streamed writes) are written once on top of it.
//! Two backends are selected at compile time:
//!
//! - **native** (`CapBackend`): a [`cap_std::fs::Dir`] *capability* — the `Dir` *is*
//!   the sandbox, so traversal and symlink escapes are rejected structurally (no
//!   `canonicalize` dance, no TOCTOU window).
//! - **wasm** (`StdBackend`): plain [`std::fs`] rooted at a path. cap-std's WASI
//!   dependencies do not yet build on stable Rust for `wasm32-wasip2`; there a host
//!   *preopen* supplies and confines the root, so the WASI runtime enforces the
//!   sandbox and no ambient authority can escape it.
//!
//! Immutable writes use `create_new` (atomic refuse-if-exists). Conditional writes use
//! a content-hash [`Version`] made atomic by a per-path in-process lock plus a
//! `<path>.lock` file (cross-process, like git's ref locks). cap-std/std::fs are
//! synchronous; the async [`FileStore`] contract is kept by running each operation
//! through `blocking` — `spawn_blocking` on native (keeping the reactor free), a
//! direct call on wasm (single-threaded, where a blocking syscall is the norm). The
//! crate is *capability-pure*: it never mints ambient authority — callers hand it an
//! open `Dir` (native) or a preopened root (wasm).

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gitana_file_store::{
	ByteReader, DeleteOutcome, FileStore, FileStoreError, Result, Version, WriteOutcome, split_prefix,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;

#[cfg(not(target_arch = "wasm32"))]
use cap_std::fs::{Dir, OpenOptions};
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::Mutex as AsyncMutex;

#[cfg(target_arch = "wasm32")]
use std::path::PathBuf;

// Linked-worktree routing is a native concern (git worktree layouts); the wasm target
// uses a single `LocalFileStore`, and cap-std's `Dir` (which this takes) is native-only.
#[cfg(not(target_arch = "wasm32"))]
mod worktree;
#[cfg(not(target_arch = "wasm32"))]
pub use worktree::WorktreeFileStore;

/// How many times to retry a cross-process ref lock before giving up.
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);
/// How many `.tmp.<n>` names to try before giving up on a unique temp file.
const TEMP_ATTEMPTS: u32 = 100;
/// Read-ahead depth (64 KiB chunks) for the streaming reader's blocking-pool pump.
#[cfg(not(target_arch = "wasm32"))]
const READ_AHEAD: usize = 4;

/// A `FileStore` rooted at a confined directory on the local filesystem.
pub struct LocalFileStore {
	backend: Arc<dyn Backend>,
	temp_counter: Arc<AtomicU64>,
	#[cfg(not(target_arch = "wasm32"))]
	locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl LocalFileStore {
	/// A store over the directory capability `dir` (native). The caller supplies the
	/// open `Dir`; this crate never mints ambient authority itself.
	#[cfg(not(target_arch = "wasm32"))]
	pub fn from_dir(dir: Dir) -> Self {
		Self::with_backend(Arc::new(CapBackend { dir }))
	}

	/// A store over the directory `root` via `std::fs` (wasm). Under `wasm32-wasip2`
	/// the host preopens `root` and the WASI runtime confines access to it.
	#[cfg(target_arch = "wasm32")]
	pub fn from_root(root: impl Into<PathBuf>) -> Self {
		Self::with_backend(Arc::new(StdBackend { root: root.into() }))
	}

	fn with_backend(backend: Arc<dyn Backend>) -> Self {
		Self {
			backend,
			temp_counter: Arc::new(AtomicU64::new(temp_seed())),
			#[cfg(not(target_arch = "wasm32"))]
			locks: Mutex::new(HashMap::new()),
		}
	}

	/// Lexically validate a git-relative path, returning it unchanged. The backend
	/// confines paths structurally; this is cheap defence-in-depth that also gives a
	/// deterministic [`FileStoreError::Backend`] for traversal/empty components.
	fn resolve<'p>(&self, path: &'p str) -> Result<&'p str> {
		for part in path.split('/') {
			if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
				return Err(FileStoreError::Backend(format!(
					"rejected unsafe path component in {path:?}"
				)));
			}
		}
		Ok(path)
	}

	/// The per-path in-process lock, serialising tasks that mutate one path.
	#[cfg(not(target_arch = "wasm32"))]
	fn lock_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
		self
			.locks
			.lock()
			.expect("lock registry poisoned")
			.entry(key.to_owned())
			.or_insert_with(|| Arc::new(AsyncMutex::new(())))
			.clone()
	}
}

impl FileStore for LocalFileStore {
	async fn read_path(&self, path: &str) -> Result<Vec<u8>> {
		let path = self.resolve(path)?.to_owned();
		let fs = Arc::clone(&self.backend);
		blocking(move || fs.read(&path).map_err(read_err)).await
	}

	async fn read_path_versioned(&self, path: &str) -> Result<(Vec<u8>, Version)> {
		let bytes = self.read_path(path).await?;
		let version = version_of(&bytes);
		Ok((bytes, version))
	}

	async fn write_path_if_absent(&self, path: &str, bytes: &[u8]) -> Result<WriteOutcome> {
		let path = self.resolve(path)?.to_owned();
		let bytes = bytes.to_vec();
		let fs = Arc::clone(&self.backend);
		blocking(move || write_if_absent(&*fs, &path, &bytes)).await
	}

	async fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> Result<Version> {
		let path = self.resolve(path)?.to_owned();
		let bytes = bytes.to_vec();
		let expected = expected.cloned();

		#[cfg(not(target_arch = "wasm32"))]
		let _task_guard = self.lock_for(&path).lock_owned().await;
		let _file_guard = LockFileGuard::acquire(Arc::clone(&self.backend), &path).await?;

		let fs = Arc::clone(&self.backend);
		let counter = Arc::clone(&self.temp_counter);
		blocking(move || {
			let current = read_current_version(&*fs, &path)?;
			if expected.as_ref() != current.as_ref() {
				return Err(FileStoreError::VersionMismatch);
			}
			write_atomic(&*fs, &counter, &path, &bytes)?;
			Ok(version_of(&bytes))
		})
		.await
	}

	async fn delete_path(&self, path: &str, expected: Option<&Version>) -> Result<DeleteOutcome> {
		let path = self.resolve(path)?.to_owned();
		let expected = expected.cloned();

		#[cfg(not(target_arch = "wasm32"))]
		let _task_guard = self.lock_for(&path).lock_owned().await;
		let _file_guard = LockFileGuard::acquire(Arc::clone(&self.backend), &path).await?;

		let fs = Arc::clone(&self.backend);
		blocking(move || {
			let current = match read_current_version(&*fs, &path)? {
				Some(version) => version,
				None => return Ok(DeleteOutcome::NotFound),
			};
			if let Some(expected) = expected
				&& expected != current
			{
				return Err(FileStoreError::VersionMismatch);
			}
			fs.remove_file(&path).map_err(backend_err)?;
			Ok(DeleteOutcome::Deleted)
		})
		.await
	}

	async fn exists(&self, path: &str) -> Result<bool> {
		let path = self.resolve(path)?.to_owned();
		let fs = Arc::clone(&self.backend);
		blocking(move || exists_at(&*fs, &path)).await
	}

	async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
		let (dir_part, frag) = split_prefix(prefix);
		let dir_rel = dir_part.trim_end_matches('/');
		if !dir_rel.is_empty() {
			self.resolve(dir_rel)?;
		}
		let dir_part = dir_part.to_owned();
		let dir_rel = dir_rel.to_owned();
		let frag = frag.to_owned();
		let fs = Arc::clone(&self.backend);
		blocking(move || list_prefix_in(&*fs, &dir_part, &dir_rel, &frag)).await
	}

	async fn read_path_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
		let path = self.resolve(path)?.to_owned();
		let fs = Arc::clone(&self.backend);
		blocking(move || fs.read_range(&path, offset, length).map_err(read_err)).await
	}

	async fn read_path_stream(&self, path: &str) -> Result<ByteReader> {
		let path = self.resolve(path)?.to_owned();
		let fs = Arc::clone(&self.backend);
		// Open the sync reader off-reactor so a missing path surfaces here, then hand back a
		// lazily-streaming adapter that pulls the rest on demand (never buffering the whole
		// value).
		let reader = blocking(move || fs.open_read(&path).map_err(read_err)).await?;
		Ok(stream_reader(reader))
	}

	async fn write_path_stream_if_absent(
		&self,
		path: &str,
		mut reader: ByteReader,
		max_len: u64,
	) -> Result<WriteOutcome> {
		let path = self.resolve(path)?.to_owned();

		// Open a temp file to stream into, bailing early if the destination already exists.
		let fs = Arc::clone(&self.backend);
		let counter = Arc::clone(&self.temp_counter);
		let open_path = path.clone();
		let opened = blocking(move || -> Result<Option<(String, Box<dyn Write + Send>)>> {
			if exists_at(&*fs, &open_path)? {
				return Ok(None);
			}
			let parent = parent_of(&open_path);
			if let Some(parent) = parent {
				fs.create_dir_all(parent).map_err(backend_err)?;
			}
			create_temp(&*fs, &counter, parent).map(Some)
		})
		.await?;
		let (temp, mut writer) = match opened {
			Some(opened) => opened,
			None => return Ok(WriteOutcome::AlreadyExists),
		};

		// Stream 64 KiB chunks straight to the temp file — never buffering the whole value —
		// enforcing `max_len` as we go. Each write moves the handle through `blocking`, so it
		// runs off-reactor on native and inline on wasm.
		let mut total: u64 = 0;
		let mut chunk = [0u8; 64 * 1024];
		loop {
			let n = match reader.read(&mut chunk).await {
				Ok(0) => break,
				Ok(n) => n,
				Err(error) => {
					cleanup_temp(Arc::clone(&self.backend), writer, temp).await;
					return Err(backend_err(error));
				}
			};
			total += n as u64;
			if total > max_len {
				cleanup_temp(Arc::clone(&self.backend), writer, temp).await;
				return Err(FileStoreError::TooLarge { limit: max_len });
			}
			let data = chunk[..n].to_vec();
			writer = blocking(move || -> Result<Box<dyn Write + Send>> {
				let mut writer = writer;
				writer.write_all(&data).map_err(backend_err)?;
				Ok(writer)
			})
			.await?;
		}

		// Flush, then publish atomically — re-checking absence for the immutable contract.
		let fs = Arc::clone(&self.backend);
		blocking(move || -> Result<WriteOutcome> {
			writer.flush().map_err(backend_err)?;
			drop(writer);
			if exists_at(&*fs, &path)? {
				let _ = fs.remove_file(&temp);
				return Ok(WriteOutcome::AlreadyExists);
			}
			fs.rename(&temp, &path).map_err(backend_err)?;
			Ok(WriteOutcome::Written)
		})
		.await
	}
}

/// Run a synchronous filesystem operation without blocking the async runtime.
///
/// On native this offloads to tokio's blocking pool (which exists independently of the
/// current-thread reactor). On wasm there is no thread pool and a blocking syscall is
/// the norm, so the closure runs inline and the future is immediately ready.
#[cfg(not(target_arch = "wasm32"))]
async fn blocking<T, F>(f: F) -> T
where
	F: FnOnce() -> T + Send + 'static,
	T: Send + 'static,
{
	tokio::task::spawn_blocking(f)
		.await
		.expect("file-store blocking task panicked")
}

#[cfg(target_arch = "wasm32")]
async fn blocking<T, F>(f: F) -> T
where
	F: FnOnce() -> T,
{
	f()
}

/// The raw synchronous filesystem primitives, path-relative to a confined root. The
/// store's atomic/CAS/streaming semantics are composed on top of these once, so only
/// the primitives differ between the native (cap-std) and wasm (`std::fs`) targets.
trait Backend: Send + Sync + 'static {
	fn read(&self, path: &str) -> std::io::Result<Vec<u8>>;
	fn read_range(&self, path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>>;
	fn create_dir_all(&self, path: &str) -> std::io::Result<()>;
	/// Open `path` for writing iff it does not exist; `Ok(None)` if it already exists.
	fn create_new(&self, path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>>;
	/// Open `path` as a sequential reader, for streaming reads.
	fn open_read(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>>;
	fn rename(&self, from: &str, to: &str) -> std::io::Result<()>;
	fn remove_file(&self, path: &str) -> std::io::Result<()>;
	fn exists(&self, path: &str) -> std::io::Result<bool>;
	/// Raw entry names directly under `dir_rel` (`""` = root); empty if the dir is absent.
	fn list_names(&self, dir_rel: &str) -> std::io::Result<Vec<String>>;
}

/// Native backend: every operation goes through a [`cap_std::fs::Dir`] capability.
#[cfg(not(target_arch = "wasm32"))]
struct CapBackend {
	dir: Dir,
}

#[cfg(not(target_arch = "wasm32"))]
impl Backend for CapBackend {
	fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
		self.dir.read(path)
	}

	fn read_range(&self, path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
		let mut file = self.dir.open(path)?;
		file.seek(SeekFrom::Start(offset))?;
		let mut buf = Vec::new();
		file.take(length).read_to_end(&mut buf)?;
		Ok(buf)
	}

	fn create_dir_all(&self, path: &str) -> std::io::Result<()> {
		self.dir.create_dir_all(path)
	}

	fn create_new(&self, path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>> {
		match self
			.dir
			.open_with(path, OpenOptions::new().write(true).create_new(true))
		{
			Ok(file) => Ok(Some(Box::new(file))),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
			Err(error) => Err(error),
		}
	}

	fn open_read(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>> {
		Ok(Box::new(self.dir.open(path)?))
	}

	fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
		self.dir.rename(from, &self.dir, to)
	}

	fn remove_file(&self, path: &str) -> std::io::Result<()> {
		self.dir.remove_file(path)
	}

	fn exists(&self, path: &str) -> std::io::Result<bool> {
		match self.dir.metadata(path) {
			Ok(_) => Ok(true),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
			Err(error) => Err(error),
		}
	}

	fn list_names(&self, dir_rel: &str) -> std::io::Result<Vec<String>> {
		let entries = if dir_rel.is_empty() {
			self.dir.entries()
		} else {
			self.dir.read_dir(dir_rel)
		};
		let entries = match entries {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(error) => return Err(error),
		};
		let mut names = Vec::new();
		for entry in entries {
			names.push(entry?.file_name().to_string_lossy().into_owned());
		}
		Ok(names)
	}
}

/// Wasm backend: `std::fs` rooted at a host-preopened directory (cap-std's WASI deps do
/// not yet build on stable Rust; WASI preopens enforce the sandbox in its place).
#[cfg(target_arch = "wasm32")]
struct StdBackend {
	root: PathBuf,
}

#[cfg(target_arch = "wasm32")]
impl StdBackend {
	fn full(&self, path: &str) -> PathBuf {
		self.root.join(path)
	}
}

#[cfg(target_arch = "wasm32")]
impl Backend for StdBackend {
	fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
		std::fs::read(self.full(path))
	}

	fn read_range(&self, path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
		let mut file = std::fs::File::open(self.full(path))?;
		file.seek(SeekFrom::Start(offset))?;
		let mut buf = Vec::new();
		file.take(length).read_to_end(&mut buf)?;
		Ok(buf)
	}

	fn create_dir_all(&self, path: &str) -> std::io::Result<()> {
		std::fs::create_dir_all(self.full(path))
	}

	fn create_new(&self, path: &str) -> std::io::Result<Option<Box<dyn Write + Send>>> {
		match std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(self.full(path))
		{
			Ok(file) => Ok(Some(Box::new(file))),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
			Err(error) => Err(error),
		}
	}

	fn open_read(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>> {
		Ok(Box::new(std::fs::File::open(self.full(path))?))
	}

	fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
		std::fs::rename(self.full(from), self.full(to))
	}

	fn remove_file(&self, path: &str) -> std::io::Result<()> {
		std::fs::remove_file(self.full(path))
	}

	fn exists(&self, path: &str) -> std::io::Result<bool> {
		match std::fs::metadata(self.full(path)) {
			Ok(_) => Ok(true),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
			Err(error) => Err(error),
		}
	}

	fn list_names(&self, dir_rel: &str) -> std::io::Result<Vec<String>> {
		let dir_full = if dir_rel.is_empty() {
			self.root.clone()
		} else {
			self.full(dir_rel)
		};
		let entries = match std::fs::read_dir(dir_full) {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(error) => return Err(error),
		};
		let mut names = Vec::new();
		for entry in entries {
			names.push(entry?.file_name().to_string_lossy().into_owned());
		}
		Ok(names)
	}
}

/// The parent of a git-relative path (everything before the last `/`), or `None` when
/// the path is a direct child of the store root.
fn parent_of(path: &str) -> Option<&str> {
	path.rfind('/').map(|i| &path[..i])
}

fn write_if_absent(fs: &dyn Backend, path: &str, bytes: &[u8]) -> Result<WriteOutcome> {
	if let Some(parent) = parent_of(path) {
		fs.create_dir_all(parent).map_err(backend_err)?;
	}
	match fs.create_new(path).map_err(backend_err)? {
		Some(mut writer) => {
			writer.write_all(bytes).map_err(backend_err)?;
			writer.flush().map_err(backend_err)?;
			Ok(WriteOutcome::Written)
		}
		None => Ok(WriteOutcome::AlreadyExists),
	}
}

/// Write `bytes` to a temp file in the destination's directory, then rename it into
/// place so a partial write never appears at `path`.
fn write_atomic(fs: &dyn Backend, counter: &AtomicU64, path: &str, bytes: &[u8]) -> Result<()> {
	let parent = parent_of(path);
	if let Some(parent) = parent {
		fs.create_dir_all(parent).map_err(backend_err)?;
	}
	let (temp, mut writer) = create_temp(fs, counter, parent)?;
	writer.write_all(bytes).map_err(backend_err)?;
	writer.flush().map_err(backend_err)?;
	drop(writer);
	fs.rename(&temp, path).map_err(backend_err)
}

/// Discard a partially-written temp file (best effort), off-reactor: drop the writer to
/// close the handle, then remove the file.
async fn cleanup_temp(fs: Arc<dyn Backend>, writer: Box<dyn Write + Send>, temp: String) {
	blocking(move || {
		drop(writer);
		let _ = fs.remove_file(&temp);
	})
	.await;
}

/// Wrap a synchronous reader as an async [`ByteReader`] that reads lazily, never buffering
/// the whole value. Native pumps 64 KiB chunks off the blocking pool through a bounded
/// channel; wasm (single-threaded, no reactor) reads inline in `poll_read`.
#[cfg(not(target_arch = "wasm32"))]
fn stream_reader(reader: Box<dyn Read + Send>) -> ByteReader {
	let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(READ_AHEAD);
	tokio::task::spawn_blocking(move || {
		let mut reader = reader;
		let mut chunk = [0u8; 64 * 1024];
		loop {
			match reader.read(&mut chunk) {
				Ok(0) => break,
				Ok(n) => {
					if tx.blocking_send(Ok(chunk[..n].to_vec())).is_err() {
						break; // the receiver was dropped
					}
				}
				Err(error) => {
					let _ = tx.blocking_send(Err(error));
					break;
				}
			}
		}
	});
	Box::new(ChannelReader {
		rx,
		leftover: Vec::new(),
		offset: 0,
	})
}

#[cfg(target_arch = "wasm32")]
fn stream_reader(reader: Box<dyn Read + Send>) -> ByteReader {
	Box::new(InlineReader { reader })
}

/// Native streaming reader: delivers chunks produced by a blocking-pool pump over a bounded
/// channel, with a small leftover buffer for partial `poll_read` fills.
#[cfg(not(target_arch = "wasm32"))]
struct ChannelReader {
	rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
	leftover: Vec<u8>,
	offset: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl AsyncRead for ChannelReader {
	fn poll_read(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		let this = self.get_mut();
		loop {
			if this.offset < this.leftover.len() {
				let n = std::cmp::min(buf.remaining(), this.leftover.len() - this.offset);
				buf.put_slice(&this.leftover[this.offset..this.offset + n]);
				this.offset += n;
				return std::task::Poll::Ready(Ok(()));
			}
			match this.rx.poll_recv(cx) {
				std::task::Poll::Ready(Some(Ok(chunk))) => {
					this.leftover = chunk;
					this.offset = 0;
				}
				std::task::Poll::Ready(Some(Err(error))) => return std::task::Poll::Ready(Err(error)),
				std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())),
				std::task::Poll::Pending => return std::task::Poll::Pending,
			}
		}
	}
}

/// Wasm streaming reader: reads synchronously inline (there is no reactor to keep free).
#[cfg(target_arch = "wasm32")]
struct InlineReader {
	reader: Box<dyn Read + Send>,
}

#[cfg(target_arch = "wasm32")]
impl AsyncRead for InlineReader {
	fn poll_read(
		self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		let this = self.get_mut();
		let dst = buf.initialize_unfilled();
		match this.reader.read(dst) {
			Ok(n) => {
				buf.advance(n);
				std::task::Poll::Ready(Ok(()))
			}
			Err(error) => std::task::Poll::Ready(Err(error)),
		}
	}
}

/// A per-process seed for the temp-file counter. Seeding from the wall clock — which
/// advances every run and never repeats, unlike a reusable pid — keeps a fresh process from
/// reprobing a crashed one's `.tmp.<n>` window and exhausting `TEMP_ATTEMPTS` against its
/// stale files. Falls back to `0` if the clock is unavailable.
fn temp_seed() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| elapsed.as_nanos() as u64)
		.unwrap_or(0)
}

/// Create a uniquely-named `.tmp.<n>` file next to a destination via `create_new`, retrying
/// on collision. The counter is seeded per process (see [`temp_seed`]), so a fresh process
/// does not reprobe a crashed one's window; atomic `create_new` handles residual collisions.
fn create_temp(
	fs: &dyn Backend,
	counter: &AtomicU64,
	parent: Option<&str>,
) -> Result<(String, Box<dyn Write + Send>)> {
	for _ in 0..TEMP_ATTEMPTS {
		let n = counter.fetch_add(1, Ordering::Relaxed);
		let name = match parent {
			Some(parent) => format!("{parent}/.tmp.{n}"),
			None => format!(".tmp.{n}"),
		};
		if let Some(writer) = fs.create_new(&name).map_err(backend_err)? {
			return Ok((name, writer));
		}
	}
	Err(FileStoreError::Backend(
		"could not create a unique temp file".to_owned(),
	))
}

fn exists_at(fs: &dyn Backend, path: &str) -> Result<bool> {
	fs.exists(path).map_err(backend_err)
}

fn list_prefix_in(
	fs: &dyn Backend,
	dir_part: &str,
	dir_rel: &str,
	frag: &str,
) -> Result<Vec<String>> {
	let mut out = Vec::new();
	for name in fs.list_names(dir_rel).map_err(backend_err)? {
		// Skip the backend's own ref-lock and temp files.
		if name.ends_with(".lock") || name.starts_with(".tmp.") {
			continue;
		}
		if name.starts_with(frag) {
			out.push(format!("{dir_part}{name}"));
		}
	}
	Ok(out)
}

/// A `<path>.lock` file held for the duration of a conditional write, giving
/// cross-process mutual exclusion. Removed on drop. A crashed holder orphans the lock
/// (manual removal required), matching git's ref-lock behaviour.
struct LockFileGuard {
	backend: Arc<dyn Backend>,
	path: String,
}

impl LockFileGuard {
	async fn acquire(backend: Arc<dyn Backend>, target: &str) -> Result<Self> {
		let path = format!("{target}.lock");
		if let Some(parent) = parent_of(&path) {
			backend.create_dir_all(parent).map_err(backend_err)?;
		}
		for _ in 0..LOCK_ATTEMPTS {
			// Creating the lock file *is* the lock; drop the returned handle immediately (the
			// file persists on disk and is removed on `Drop`). Binding to a bool also keeps the
			// non-`Send` handle from straddling the `.await`, so the future stays `Send`.
			let acquired = backend.create_new(&path).map_err(backend_err)?.is_some();
			if acquired {
				return Ok(LockFileGuard { backend, path });
			}
			lock_backoff().await;
		}
		Err(FileStoreError::Backend(format!(
			"{target} is locked by another process"
		)))
	}
}

impl Drop for LockFileGuard {
	fn drop(&mut self) {
		let _ = self.backend.remove_file(&self.path);
	}
}

/// Wait before retrying a contended ref lock. Native sleeps on the blocking pool (so the
/// reactor stays free without a tokio timer); wasm has no sleep primitive without a host
/// clock, and is single-process here, so it retries immediately.
#[cfg(not(target_arch = "wasm32"))]
async fn lock_backoff() {
	blocking(|| std::thread::sleep(LOCK_BACKOFF)).await;
}

#[cfg(target_arch = "wasm32")]
async fn lock_backoff() {
	let _ = LOCK_BACKOFF;
}

fn read_current_version(fs: &dyn Backend, path: &str) -> Result<Option<Version>> {
	match fs.read(path) {
		Ok(bytes) => Ok(Some(version_of(&bytes))),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(backend_err(error)),
	}
}

/// A content-hash version token: the SHA-256 hex of the stored bytes.
fn version_of(bytes: &[u8]) -> Version {
	let digest = Sha256::digest(bytes);
	let mut hex = String::with_capacity(64);
	for byte in digest {
		hex.push_str(&format!("{byte:02x}"));
	}
	Version(hex.into())
}

fn backend_err(error: std::io::Error) -> FileStoreError {
	FileStoreError::Backend(error.to_string())
}

fn read_err(error: std::io::Error) -> FileStoreError {
	match error.kind() {
		std::io::ErrorKind::NotFound => FileStoreError::NotFound,
		_ => FileStoreError::Backend(error.to_string()),
	}
}
