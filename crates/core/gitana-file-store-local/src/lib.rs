//! Filesystem [`FileStore`] backend for local / standalone deployments.
//!
//! Maps each repository to `<root>/<repo-id>/` and stores git-relative paths
//! beneath it. Immutable writes use `create_new` (atomic refuse-if-exists).
//! Conditional writes use a content-hash [`Version`] and are made atomic by a
//! per-path in-process lock (serialises tasks) plus a `<path>.lock` file
//! (serialises across processes on the host, like git's ref locks). Paths are
//! validated lexically and checked against the canonicalised store root to reject
//! traversal and symlink escapes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gitana_file_store::{
	ByteReader, DeleteOutcome, FileStore, FileStoreError, Result, Version, WriteOutcome, split_prefix,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

/// How long to wait for a cross-process ref lock before giving up.
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// A `FileStore` rooted at a directory on the local filesystem.
pub struct LocalFileStore {
	root: PathBuf,
	temp_counter: AtomicU64,
	locks: Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
}

impl LocalFileStore {
	/// A store that keeps repositories under `root`.
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self {
			root: root.into(),
			temp_counter: AtomicU64::new(0),
			locks: Mutex::new(HashMap::new()),
		}
	}

	/// Lexically validate a git-relative path and join it under the store root.
	fn resolve(&self, path: &str) -> Result<PathBuf> {
		for part in path.split('/') {
			if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
				return Err(FileStoreError::Backend(format!(
					"rejected unsafe path component in {path:?}"
				)));
			}
		}
		Ok(self.root.join(path))
	}

	/// Reject a resolved path whose deepest existing ancestor canonicalises outside
	/// the store root (defends against symlink escape).
	async fn ensure_within_root(&self, full: &Path) -> Result<()> {
		let canon_root = match tokio::fs::canonicalize(&self.root).await {
			Ok(root) => root,
			// Root does not exist yet → nothing is stored → nothing can escape.
			Err(_) => return Ok(()),
		};

		let mut current = full;
		loop {
			if tokio::fs::try_exists(current).await.unwrap_or(false) {
				let canon = tokio::fs::canonicalize(current).await.map_err(backend)?;
				return if canon.starts_with(&canon_root) {
					Ok(())
				} else {
					Err(FileStoreError::Backend(format!(
						"path escapes store root: {}",
						full.display()
					)))
				};
			}
			match current.parent() {
				Some(parent) => current = parent,
				None => return Ok(()),
			}
		}
	}

	/// The per-path in-process lock, serialising tasks that mutate one path.
	fn lock_for(&self, key: &Path) -> Arc<AsyncMutex<()>> {
		self
			.locks
			.lock()
			.expect("lock registry poisoned")
			.entry(key.to_path_buf())
			.or_insert_with(|| Arc::new(AsyncMutex::new(())))
			.clone()
	}

	async fn write_atomic(&self, full: &Path, bytes: &[u8]) -> Result<()> {
		let parent = full
			.parent()
			.ok_or_else(|| FileStoreError::Backend("path has no parent".to_owned()))?;
		tokio::fs::create_dir_all(parent).await.map_err(backend)?;

		let counter = self.temp_counter.fetch_add(1, Ordering::Relaxed);
		let temp = parent.join(format!(".tmp.{}.{}", std::process::id(), counter));
		tokio::fs::write(&temp, bytes).await.map_err(backend)?;
		tokio::fs::rename(&temp, full).await.map_err(backend)
	}
}

impl FileStore for LocalFileStore {
	async fn read_path(&self, path: &str) -> Result<Vec<u8>> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		tokio::fs::read(full).await.map_err(read_error)
	}

	async fn read_path_versioned(&self, path: &str) -> Result<(Vec<u8>, Version)> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		let bytes = tokio::fs::read(full).await.map_err(read_error)?;
		let version = version_of(&bytes);
		Ok((bytes, version))
	}

	async fn write_path_if_absent(&self, path: &str, bytes: &[u8]) -> Result<WriteOutcome> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		if let Some(parent) = full.parent() {
			tokio::fs::create_dir_all(parent).await.map_err(backend)?;
		}
		match tokio::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&full)
			.await
		{
			Ok(mut file) => {
				file.write_all(bytes).await.map_err(backend)?;
				file.flush().await.map_err(backend)?;
				Ok(WriteOutcome::Written)
			}
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				Ok(WriteOutcome::AlreadyExists)
			}
			Err(error) => Err(backend(error)),
		}
	}

	async fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> Result<Version> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;

		let lock = self.lock_for(&full);
		let _task_guard = lock.lock().await;
		let _file_guard = LockFileGuard::acquire(&full).await?;

		let current = read_current_version(&full).await?;
		if expected != current.as_ref() {
			return Err(FileStoreError::VersionMismatch);
		}
		self.write_atomic(&full, bytes).await?;
		Ok(version_of(bytes))
	}

	async fn delete_path(&self, path: &str, expected: Option<&Version>) -> Result<DeleteOutcome> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;

		let lock = self.lock_for(&full);
		let _task_guard = lock.lock().await;
		let _file_guard = LockFileGuard::acquire(&full).await?;

		let current = match read_current_version(&full).await? {
			Some(version) => version,
			None => return Ok(DeleteOutcome::NotFound),
		};
		if let Some(expected) = expected
			&& *expected != current
		{
			return Err(FileStoreError::VersionMismatch);
		}
		tokio::fs::remove_file(&full).await.map_err(backend)?;
		Ok(DeleteOutcome::Deleted)
	}

	async fn exists(&self, path: &str) -> Result<bool> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		tokio::fs::try_exists(full).await.map_err(backend)
	}

	async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
		let (dir, frag) = split_prefix(prefix);
		let dir_rel = dir.trim_end_matches('/');
		let dir_full = if dir_rel.is_empty() {
			self.root.clone()
		} else {
			self.resolve(dir_rel)?
		};
		self.ensure_within_root(&dir_full).await?;

		let mut entries = match tokio::fs::read_dir(&dir_full).await {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(error) => return Err(backend(error)),
		};

		let mut out = Vec::new();
		while let Some(entry) = entries.next_entry().await.map_err(backend)? {
			let name = entry.file_name().to_string_lossy().into_owned();
			// Skip the backend's own ref-lock and temp files.
			if name.ends_with(".lock") || name.starts_with(".tmp.") {
				continue;
			}
			if name.starts_with(frag) {
				out.push(format!("{dir}{name}"));
			}
		}
		Ok(out)
	}

	async fn read_path_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		let mut file = tokio::fs::File::open(full).await.map_err(read_error)?;
		file
			.seek(std::io::SeekFrom::Start(offset))
			.await
			.map_err(backend)?;
		let mut buf = Vec::new();
		file
			.take(length)
			.read_to_end(&mut buf)
			.await
			.map_err(backend)?;
		Ok(buf)
	}

	async fn read_path_stream(&self, path: &str) -> Result<ByteReader> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		let file = tokio::fs::File::open(full).await.map_err(read_error)?;
		Ok(Box::new(file))
	}

	async fn write_path_stream_if_absent(
		&self,
		path: &str,
		mut reader: ByteReader,
		max_len: u64,
	) -> Result<WriteOutcome> {
		let full = self.resolve(path)?;
		self.ensure_within_root(&full).await?;
		if tokio::fs::try_exists(&full).await.map_err(backend)? {
			return Ok(WriteOutcome::AlreadyExists);
		}
		let parent = full
			.parent()
			.ok_or_else(|| FileStoreError::Backend("path has no parent".to_owned()))?;
		tokio::fs::create_dir_all(parent).await.map_err(backend)?;

		// Stream to a temp file, then rename into place so a partial write never
		// appears at the destination.
		let counter = self.temp_counter.fetch_add(1, Ordering::Relaxed);
		let temp = parent.join(format!(".tmp.{}.{}", std::process::id(), counter));
		let mut file = tokio::fs::File::create(&temp).await.map_err(backend)?;

		let mut total = 0u64;
		let mut chunk = [0u8; 64 * 1024];
		loop {
			let n = reader.read(&mut chunk).await.map_err(backend)?;
			if n == 0 {
				break;
			}
			total += n as u64;
			if total > max_len {
				let _ = tokio::fs::remove_file(&temp).await;
				return Err(FileStoreError::TooLarge { limit: max_len });
			}
			file.write_all(&chunk[..n]).await.map_err(backend)?;
		}
		file.flush().await.map_err(backend)?;
		drop(file);

		if tokio::fs::try_exists(&full).await.map_err(backend)? {
			let _ = tokio::fs::remove_file(&temp).await;
			return Ok(WriteOutcome::AlreadyExists);
		}
		tokio::fs::rename(&temp, &full).await.map_err(backend)?;
		Ok(WriteOutcome::Written)
	}
}

/// A `<path>.lock` file held for the duration of a conditional write, giving
/// cross-process mutual exclusion. Removed on drop. A crashed holder orphans the
/// lock (manual removal required), matching git's ref-lock behaviour.
struct LockFileGuard {
	path: PathBuf,
}

impl LockFileGuard {
	async fn acquire(target: &Path) -> Result<Self> {
		let path = lock_path(target);
		if let Some(parent) = path.parent() {
			tokio::fs::create_dir_all(parent).await.map_err(backend)?;
		}
		for _ in 0..LOCK_ATTEMPTS {
			match tokio::fs::OpenOptions::new()
				.write(true)
				.create_new(true)
				.open(&path)
				.await
			{
				Ok(_) => return Ok(LockFileGuard { path }),
				Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
					tokio::time::sleep(LOCK_BACKOFF).await;
				}
				Err(error) => return Err(backend(error)),
			}
		}
		Err(FileStoreError::Backend(format!(
			"{} is locked by another process",
			target.display()
		)))
	}
}

impl Drop for LockFileGuard {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.path);
	}
}

fn lock_path(target: &Path) -> PathBuf {
	let mut name = target.file_name().unwrap_or_default().to_os_string();
	name.push(".lock");
	target.with_file_name(name)
}

async fn read_current_version(full: &Path) -> Result<Option<Version>> {
	match tokio::fs::read(full).await {
		Ok(bytes) => Ok(Some(version_of(&bytes))),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(backend(error)),
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

fn backend(error: std::io::Error) -> FileStoreError {
	FileStoreError::Backend(error.to_string())
}

fn read_error(error: std::io::Error) -> FileStoreError {
	match error.kind() {
		std::io::ErrorKind::NotFound => FileStoreError::NotFound,
		_ => FileStoreError::Backend(error.to_string()),
	}
}
