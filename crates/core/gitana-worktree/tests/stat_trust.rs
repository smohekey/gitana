#![cfg(unix)]
//! When a stat can vouch for a file, and the index lock across the awaits after `rm` and `mv` have
//! changed the working tree. Two test doubles: a capability that reports only a file's size, as
//! gitana's non-Unix one does, and a file store that never answers when the index was written.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gitana_file_store::{
	ByteReader, DeleteOutcome, DurabilityTarget, FileStore, PathLock, Result, Version, WriteOutcome,
};
use gitana_file_store_local::{
	CapWorkDir, DirEntry, LocalFileStore, Meta, PopulationEntry, WorkDirFs,
};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_path::{GitPath, GitPathComponent};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// On a capability that reports only a file's size (no times or identity, as on non-Unix hosts),
/// a stat never vouches for a file: a same-length edit is seen by status, staged by `add`, and
/// undone by `restore`.
#[tokio::test]
async fn a_size_only_capability_reads_files_rather_than_trust_a_stat() {
	let Some(work) = repository("size-only") else {
		return;
	};
	let wt = WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(work.join(".git")),
		))),
		SizeOnlyWorkDir(CapWorkDir::from_dir(open_dir(&work))),
		work.join(".git"),
	);
	// gitana's own checkout, so the index records the size-only stat.
	let head = wt
		.repository()
		.refs()
		.resolve_head()
		.await
		.unwrap()
		.unwrap();
	let tree = wt.repository().commit_tree(head).await.unwrap();
	wt.checkout(tree, true, None).await.unwrap();

	std::fs::write(work.join("f.txt"), b"bbbb").unwrap();
	let changed: Vec<String> = wt
		.status(None)
		.await
		.unwrap()
		.changed
		.iter()
		.filter(|entry| entry.worktree != ' ')
		.map(|entry| entry.path.to_string())
		.collect();
	assert_eq!(changed, ["f.txt"], "status");
	wt.restore(None, true, false, &["f.txt"], "").await.unwrap();
	assert_eq!(
		std::fs::read(work.join("f.txt")).unwrap(),
		b"aaaa",
		"restore"
	);

	std::fs::write(work.join("f.txt"), b"cccc").unwrap();
	wt.add(&["f.txt"], "", false, None).await.unwrap();
	assert_eq!(git(&work, &["cat-file", "blob", ":f.txt"]), "cccc", "add");
}

/// Cancelled while it writes the index, after its files are gone, `rm` keeps `index.lock`: the
/// working tree no longer matches the index, so nothing may take the lock as if it did.
#[tokio::test]
async fn rm_cancelled_writing_the_index_keeps_the_lock() {
	let Some(work) = repository("rm-cancelled") else {
		return;
	};
	// `rm` asks the index's time once before it starts; the index write asks again, and waits.
	let wt = pending_after(&work, 1);
	let cancelled = tokio::time::timeout(
		Duration::from_millis(500),
		wt.rm(&["f.txt"], "", false, false, false, false),
	)
	.await;
	assert!(cancelled.is_err(), "still waiting when dropped");
	assert!(!work.join("f.txt").exists(), "the file is gone");
	assert!(work.join(".git/index.lock").exists(), "the lock is kept");
}

/// Cancelled while it settles the moved entries' stats, after its files have moved, `mv` keeps
/// `index.lock`.
#[tokio::test]
async fn mv_cancelled_after_moving_keeps_the_lock() {
	let Some(work) = repository("mv-cancelled") else {
		return;
	};
	// `mv` first asks the index's time once its files have moved.
	let wt = pending_after(&work, 0);
	let cancelled = tokio::time::timeout(
		Duration::from_millis(500),
		wt.mv(&["f.txt"], "g.txt", "", false, false),
	)
	.await;
	assert!(cancelled.is_err(), "still waiting when dropped");
	assert!(work.join("g.txt").exists(), "the file moved");
	assert!(work.join(".git/index.lock").exists(), "the lock is kept");
}

/// A conversion setting `mv` can't read fails it before anything moves: the file, the index and
/// the lock are as they were.
#[tokio::test]
async fn mv_with_a_bad_setting_moves_nothing() {
	let Some(work) = repository("mv-bad-setting") else {
		return;
	};
	git(&work, &["config", "core.autocrlf", "maybe"]);
	let wt = WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(work.join(".git")),
		))),
		CapWorkDir::from_dir(open_dir(&work)),
		work.join(".git"),
	);
	let index = std::fs::read(work.join(".git/index")).unwrap();
	assert!(wt.mv(&["f.txt"], "g.txt", "", false, false).await.is_err());
	assert!(work.join("f.txt").exists() && !work.join("g.txt").exists());
	assert_eq!(std::fs::read(work.join(".git/index")).unwrap(), index);
	assert!(!work.join(".git/index.lock").exists());
}

// --- doubles -------------------------------------------------------------------------------------

/// A capability reporting only a file's size and kind, as gitana's non-Unix one does.
struct SizeOnlyWorkDir(CapWorkDir);

impl WorkDirFs for SizeOnlyWorkDir {
	fn uses_symlink_placeholders(&self) -> bool {
		self.0.uses_symlink_placeholders()
	}

	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()> {
		self.0.validate_path_representable(path)
	}

	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>> {
		Ok(self.0.lstat(path)?.map(|meta| Meta {
			kind: meta.kind,
			size: meta.size,
			mtime: (0, 0),
			ctime: (0, 0),
			mode: 0,
			dev: 0,
			ino: 0,
			uid: 0,
			gid: 0,
		}))
	}

	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.0.read(path)
	}

	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.0.read_link(path)
	}

	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>> {
		self.0.read_dir(path)
	}

	fn write(&self, path: &GitPath, bytes: &[u8], executable: bool) -> io::Result<()> {
		self.0.write(path, bytes, executable)
	}

	fn populate_new(&self, path: &GitPath, entry: PopulationEntry<'_>) -> io::Result<bool> {
		self.0.populate_new(path, entry)
	}

	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()> {
		self.0.symlink(target, path)
	}

	fn create_dir(&self, path: &GitPath) -> io::Result<()> {
		self.0.create_dir(path)
	}

	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()> {
		self.0.rename(from, to)
	}

	fn remove_file(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_file(path)
	}

	fn remove_dir(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_dir(path)
	}

	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_dir_all(path)
	}
}

/// A local store that answers when a file was modified `answers` times, then never again: an
/// operation waiting on it can be dropped there.
struct PendingTimeStore {
	inner: LocalFileStore,
	answers: Arc<AtomicUsize>,
}

fn pending_after(work: &Path, answers: usize) -> WorkTree<PendingTimeStore, CapWorkDir, Sha256> {
	let store = PendingTimeStore {
		inner: LocalFileStore::from_dir(open_dir(work.join(".git"))),
		answers: Arc::new(AtomicUsize::new(answers)),
	};
	WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(store)),
		CapWorkDir::from_dir(open_dir(work)),
		work.join(".git"),
	)
}

impl FileStore for PendingTimeStore {
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			inner: self.inner.shared_handle(),
			answers: Arc::clone(&self.answers),
		}
	}

	fn durability_barrier(
		&self,
		targets: &[DurabilityTarget],
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.durability_barrier(targets)
	}

	fn read_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_path(path)
	}

	fn read_shared_path(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_shared_path(path)
	}

	fn read_path_bytes(&self, path: &GitPath) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_path_bytes(path)
	}

	fn is_dir_bytes(&self, path: &GitPath) -> impl Future<Output = Result<bool>> + Send {
		self.inner.is_dir_bytes(path)
	}

	fn list_prefix_bytes(
		&self,
		prefix: &GitPath,
	) -> impl Future<Output = Result<Vec<GitPathComponent>>> + Send {
		self.inner.list_prefix_bytes(prefix)
	}

	fn read_path_versioned(
		&self,
		path: &str,
	) -> impl Future<Output = Result<(Vec<u8>, Version)>> + Send {
		self.inner.read_path_versioned(path)
	}

	fn write_path_if_absent(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<WriteOutcome>> + Send {
		self.inner.write_path_if_absent(path, bytes)
	}

	fn try_lock_path(&self, path: &str) -> impl Future<Output = Result<Option<PathLock>>> + Send {
		self.inner.try_lock_path(path)
	}

	fn write_path_cas(
		&self,
		path: &str,
		bytes: &[u8],
		expected: Option<&Version>,
	) -> impl Future<Output = Result<Version>> + Send {
		self.inner.write_path_cas(path, bytes, expected)
	}

	fn write_path_replace(
		&self,
		path: &str,
		bytes: &[u8],
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.write_path_replace(path, bytes)
	}

	fn delete_path(
		&self,
		path: &str,
		expected: Option<&Version>,
	) -> impl Future<Output = Result<DeleteOutcome>> + Send {
		self.inner.delete_path(path, expected)
	}

	fn delete_path_unlocked(&self, path: &str) -> impl Future<Output = Result<DeleteOutcome>> + Send {
		self.inner.delete_path_unlocked(path)
	}

	fn remove_dir(&self, path: &str) -> impl Future<Output = Result<()>> + Send {
		self.inner.remove_dir(path)
	}

	fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> + Send {
		self.inner.exists(path)
	}

	async fn modified(&self, path: &str) -> Result<Option<std::time::SystemTime>> {
		let answer = self
			.answers
			.try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
				left.checked_sub(1)
			})
			.is_ok();
		if !answer {
			std::future::pending::<()>().await;
		}
		self.inner.modified(path).await
	}

	fn is_dir(&self, path: &str) -> impl Future<Output = Result<bool>> + Send {
		self.inner.is_dir(path)
	}

	fn size(&self, path: &str) -> impl Future<Output = Result<u64>> + Send {
		self.inner.size(path)
	}

	fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send {
		self.inner.list_prefix(prefix)
	}

	fn list_prefix_bounded(
		&self,
		prefix: &str,
		max_entries: usize,
		max_bytes: u64,
	) -> impl Future<Output = Result<Vec<String>>> + Send {
		self
			.inner
			.list_prefix_bounded(prefix, max_entries, max_bytes)
	}

	fn read_path_range(
		&self,
		path: &str,
		offset: u64,
		length: u64,
	) -> impl Future<Output = Result<Vec<u8>>> + Send {
		self.inner.read_path_range(path, offset, length)
	}

	fn read_path_stream(&self, path: &str) -> impl Future<Output = Result<ByteReader>> + Send {
		self.inner.read_path_stream(path)
	}

	fn write_path_stream_if_absent(
		&self,
		path: &str,
		reader: ByteReader,
		max_len: u64,
	) -> impl Future<Output = Result<WriteOutcome>> + Send {
		self
			.inner
			.write_path_stream_if_absent(path, reader, max_len)
	}

	fn remove_lock_file_sync(&self, path: &str) {
		self.inner.remove_lock_file_sync(path)
	}

	fn replace_and_release_lock(
		&self,
		path: &str,
		bytes: &[u8],
		lock_path: &str,
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.replace_and_release_lock(path, bytes, lock_path)
	}
}

// --- helpers -------------------------------------------------------------------------------------

/// A repository at a new temporary folder with `f.txt` (`aaaa`) committed; `None` where git can't
/// make sha256 repositories.
fn repository(tag: &str) -> Option<PathBuf> {
	let work = unique_tmp(tag);
	let probe = Command::new("git")
		.args(["init", "--object-format=sha256", "-q", "-b", "main"])
		.arg(&work)
		.output()
		.ok()?;
	if !probe.status.success() {
		eprintln!("skipping: git without --object-format=sha256");
		return None;
	}
	git(&work, &["config", "core.autocrlf", "false"]);
	std::fs::write(work.join("f.txt"), b"aaaa").unwrap();
	git(&work, &["add", "f.txt"]);
	git(
		&work,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"f",
		],
	);
	Some(work)
}

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn git(work: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.arg("-C")
		.arg(work)
		.args(args)
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap()
}

fn unique_tmp(tag: &str) -> PathBuf {
	static SEQ: AtomicUsize = AtomicUsize::new(0);
	let dir = std::env::temp_dir().join(format!(
		"gitana-stat-trust-{tag}-{}-{}",
		std::process::id(),
		SEQ.fetch_add(1, Ordering::Relaxed)
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}
