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
	CapWorkDir, DirEntry, LocalFileStore, Meta, PopulationEntry, WorkDirFs, WorkEntryIdentity,
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

/// Cancelled while it writes the index, after its files are written, a stash pop keeps `index.lock`
/// and the stash: the working tree no longer matches the index, and the stash is dropped only after.
/// Both for a tracked change (written by the merge) and an untracked file alone (by the pop).
#[tokio::test]
async fn stash_pop_cancelled_writing_the_index_keeps_the_lock_and_the_stash() {
	for (tag, path, push) in [
		("pop-cancelled", "f.txt", &["stash", "-q"][..]),
		(
			"pop-cancelled-untracked",
			"u.txt",
			&["stash", "-q", "-u"][..],
		),
	] {
		let Some(work) = repository(tag) else {
			return;
		};
		std::fs::write(work.join(path), b"bbbb").unwrap();
		let mut args = vec!["-c", "user.name=T", "-c", "user.email=t@e"];
		args.extend(push);
		git(&work, &args);
		// The apply asks the index's time once before it starts; the index write asks again, and
		// waits.
		let wt = pending_after(&work, 1);
		let cancelled = tokio::time::timeout(
			Duration::from_millis(500),
			wt.stash_pop(&gitana_worktree::StashApply::default()),
		)
		.await;
		assert!(cancelled.is_err(), "{path}: still waiting when dropped");
		assert_eq!(
			std::fs::read(work.join(path)).unwrap(),
			b"bbbb",
			"{path}: the file is back"
		);
		assert!(
			work.join(".git/index.lock").exists(),
			"{path}: the lock is kept"
		);
		assert_eq!(
			git(&work, &["stash", "list", "--format=%gd"]).trim(),
			"stash@{0}",
			"{path}: the stash is kept"
		);
	}
}

/// A stash push holds the index lock from its save through its clean-up. Cancelled before anything
/// has changed, it leaves nothing (no stash, the lock released); cancelled while it writes the index,
/// after the stash is stored and the working tree put back, it keeps `index.lock` — also when only
/// untracked files were put away, which no checkout or restore follows.
#[tokio::test]
async fn stash_push_cancelled_keeps_the_lock_only_once_the_tree_has_changed() {
	// (tag, answers before waiting, the file changed, the pathspecs, with untracked files)
	let cases: [(&str, usize, &str, &[&str], bool); 3] = [
		("push-cancelled-early", 0, "f.txt", &[], false),
		("push-cancelled-late", 1, "f.txt", &[], false),
		("push-cancelled-untracked", 1, "u.txt", &["u.txt"], true),
	];
	for (tag, answers, path, pathspecs, include_untracked) in cases {
		let Some(work) = repository(tag) else {
			return;
		};
		std::fs::write(work.join(path), b"bbbb").unwrap();
		// The push asks the index's time once, under the lock, before it saves anything; the index
		// write asks again.
		let wt = pending_after(&work, answers);
		let request = gitana_worktree::StashPush {
			pathspecs: pathspecs
				.iter()
				.map(|spec| gitana_path::GitPathspec::from_utf8(spec).unwrap())
				.collect(),
			prefix: GitPath::root(),
			message: None,
			include_untracked,
			excludes_file: None,
		};
		let cancelled = tokio::time::timeout(
			Duration::from_millis(500),
			wt.stash_push(&request, &FixedIdentity),
		)
		.await;
		assert!(cancelled.is_err(), "{tag}: still waiting when dropped");
		let stashes = git(&work, &["stash", "list", "--format=%gd"]);
		let file = std::fs::read(work.join(path)).ok();
		let locked = work.join(".git/index.lock").exists();
		let expected: (&str, Option<&[u8]>, bool) = match (answers, include_untracked) {
			(0, _) => ("", Some(b"bbbb"), false),
			(_, false) => ("stash@{0}", Some(b"aaaa"), true),
			(_, true) => ("stash@{0}", None, true),
		};
		assert_eq!((stashes.trim(), file.as_deref(), locked), expected, "{tag}");
	}
}

/// While a push puts the working tree back, `HEAD` is held: another program can't move the branch
/// meanwhile (its ref locks are taken), so the tree and `HEAD` can't end on different commits.
#[tokio::test]
async fn stash_push_holds_head_through_its_clean_up() {
	let Some(work) = repository("push-holds-head") else {
		return;
	};
	git(
		&work,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"--allow-empty",
			"-m",
			"other",
		],
	);
	let other = git(&work, &["rev-parse", "HEAD"]);
	git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
	let before = git(&work, &["rev-parse", "HEAD"]);
	std::fs::write(work.join("f.txt"), b"bbbb").unwrap();
	// Paused writing the index, after the stash is stored and the working tree put back.
	let wt = pending_after(&work, 1);
	let request = gitana_worktree::StashPush {
		pathspecs: Vec::new(),
		prefix: GitPath::root(),
		message: None,
		include_untracked: false,
		excludes_file: None,
	};
	let mut push = Box::pin(wt.stash_push(&request, &FixedIdentity));
	assert!(
		tokio::time::timeout(Duration::from_millis(500), &mut push)
			.await
			.is_err()
	);
	let moved = Command::new("git")
		.arg("-C")
		.arg(&work)
		.args(["update-ref", "refs/heads/main", other.trim()])
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.unwrap();
	assert!(
		!moved.status.success(),
		"the branch moved while the push held HEAD"
	);
	assert_eq!(git(&work, &["rev-parse", "HEAD"]), before);
	drop(push);
}

/// A pop cancelled between creating two untracked files leaves neither behind, and the index lock
/// released: the files it created are removed as it's dropped, and the stash is kept.
#[tokio::test]
async fn stash_pop_cancelled_creating_untracked_files_leaves_none_behind() {
	let Some(work) = repository("pop-cancelled-creating") else {
		return;
	};
	std::fs::write(work.join("u1.txt"), b"one").unwrap();
	std::fs::write(work.join("u2.txt"), b"two").unwrap();
	git(
		&work,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"stash",
			"-q",
			"-u",
		],
	);
	let blob = git(&work, &["rev-parse", "stash^3:u2.txt"]);
	let blob = blob.trim();
	let loose = format!("objects/{}/{}", &blob[..2], &blob[2..]);
	// The second read of `u2.txt`'s blob: the first is the pop's check, the second its creation,
	// once `u1.txt` has been created.
	let wt = pending_on_read(&work, &loose, 2);
	let cancelled = tokio::time::timeout(
		Duration::from_millis(500),
		wt.stash_pop(&gitana_worktree::StashApply::default()),
	)
	.await;
	assert!(cancelled.is_err(), "still waiting when dropped");
	assert!(
		!work.join("u1.txt").exists(),
		"the file it created is removed"
	);
	assert!(
		std::fs::read_dir(&work).unwrap().all(|entry| !entry
			.unwrap()
			.file_name()
			.to_string_lossy()
			.starts_with(".gitana")),
		"no private names left behind"
	);
	assert!(!work.join("u2.txt").exists());
	assert!(
		!work.join(".git/index.lock").exists(),
		"the lock is released"
	);
	assert_eq!(
		git(&work, &["stash", "list", "--format=%gd"]).trim(),
		"stash@{0}",
		"the stash is kept"
	);
}

/// A fixed identity, for the commits a stash records.
struct FixedIdentity;

impl gitana_identity::Identity for FixedIdentity {
	async fn author(&self) -> anyhow::Result<String> {
		Ok("T <t@e> 1700000000 +0000".to_owned())
	}
	async fn committer(&self) -> anyhow::Result<String> {
		Ok("T <t@e> 1700000000 +0000".to_owned())
	}
	async fn committer_or_default(&self) -> anyhow::Result<String> {
		self.committer().await
	}
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

/// A refused pop's rollback leaves alone a restored file another program replaced, or rewrote in
/// place, before the refusal, even the moment it was created: only the files the pop wrote,
/// unchanged, are removed again, and no private names are left behind.
#[tokio::test]
async fn stash_pop_rollback_keeps_a_file_another_program_changed() {
	for change in [
		Change::Replace,
		Change::Rewrite,
		Change::SwapAtCreation,
		Change::RewriteKeepingTime,
	] {
		let Some(work) = repository("pop-rollback-changed") else {
			return;
		};
		std::fs::write(work.join("u1.txt"), b"one").unwrap();
		std::fs::write(work.join("u2.txt"), b"two").unwrap();
		git(
			&work,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"stash",
				"-q",
				"-u",
			],
		);
		let wt = WorkTree::new(
			Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
				open_dir(work.join(".git")),
			))),
			InterferingWorkDir {
				inner: CapWorkDir::from_dir(open_dir(&work)),
				root: work.clone(),
				change,
			},
			work.join(".git"),
		);
		let error = wt
			.stash_pop(&gitana_worktree::StashApply::default())
			.await
			.unwrap_err();
		assert!(
			matches!(
				&error,
				gitana_worktree::StashPopError::Apply(
					gitana_worktree::StashApplyError::UntrackedExists(path)
				) if path.as_bytes() == b"u2.txt"
			),
			"{error:?}"
		);
		let theirs: &[u8] = match change {
			Change::Replace | Change::SwapAtCreation => b"replaced",
			Change::Rewrite => b"rewritten in place",
			Change::RewriteKeepingTime => b"ONE",
		};
		assert_eq!(
			std::fs::read(work.join("u1.txt")).unwrap(),
			theirs,
			"{change:?}"
		);
		assert_eq!(std::fs::read(work.join("u2.txt")).unwrap(), b"theirs");
		let mut names: Vec<String> = std::fs::read_dir(&work)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
			.collect();
		names.sort();
		assert_eq!(names, [".git", "f.txt", "u1.txt", "u2.txt"], "{change:?}");
		assert_eq!(
			git(&work, &["stash", "list", "--format=%gd"]).trim(),
			"stash@{0}",
			"the stash is kept"
		);
	}
}

/// A pop of `stash@{1}` while another program drops `stash@{0}`, between the apply and the drop:
/// the stash it applied is no longer at its place, so it isn't dropped, the pop says so, and the
/// stash is kept.
#[tokio::test]
async fn stash_pop_reports_a_drop_it_could_not_make_after_another_drop() {
	let Some(work) = repository("pop-concurrent-drop") else {
		return;
	};
	let stash = |content: &[u8]| {
		std::fs::write(work.join("f.txt"), content).unwrap();
		git(
			&work,
			&["-c", "user.name=T", "-c", "user.email=t@e", "stash", "-q"],
		);
	};
	stash(b"one");
	stash(b"two");
	let older = git(&work, &["rev-parse", "stash@{1}"]);
	let other = work.clone();
	let wt = intervening_on_lock(&work, "refs/stash.lock", move || {
		git(&other, &["stash", "drop", "-q", "stash@{0}"]);
	});
	let error = wt
		.stash_pop(&gitana_worktree::StashApply {
			index: 1,
			..Default::default()
		})
		.await
		.unwrap_err();
	assert!(
		matches!(
			error,
			gitana_worktree::StashPopError::Drop {
				error: gitana_repository::RepositoryError::RefMoved { .. },
				..
			}
		),
		"{error:?}"
	);
	assert_eq!(
		git(&work, &["rev-parse", "stash@{0}"]),
		older,
		"the stash applied is kept"
	);
	assert_eq!(git(&work, &["stash", "list"]).lines().count(), 1);
	assert_eq!(std::fs::read(work.join("f.txt")).unwrap(), b"one");
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

/// A local store that pauses, never to answer, on the `pend_at`-th read of `path`: an operation
/// waiting on it can be dropped there. With `on_lock`, another program acts the first time a path
/// is locked, just before the lock is taken.
struct PendingReadStore {
	inner: LocalFileStore,
	path: String,
	reads: Arc<AtomicUsize>,
	pend_at: usize,
	on_lock: Option<Arc<LockHook>>,
}

/// What another program does the first time `path` is locked, before the lock is taken.
struct LockHook {
	path: String,
	action: Box<dyn Fn() + Send + Sync>,
	fired: std::sync::atomic::AtomicBool,
}

fn intervening_on_lock(
	work: &Path,
	path: &str,
	action: impl Fn() + Send + Sync + 'static,
) -> WorkTree<PendingReadStore, CapWorkDir, Sha256> {
	let store = PendingReadStore {
		inner: LocalFileStore::from_dir(open_dir(work.join(".git"))),
		path: String::new(),
		reads: Arc::new(AtomicUsize::new(0)),
		pend_at: usize::MAX,
		on_lock: Some(Arc::new(LockHook {
			path: path.to_owned(),
			action: Box::new(action),
			fired: std::sync::atomic::AtomicBool::new(false),
		})),
	};
	WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(store)),
		CapWorkDir::from_dir(open_dir(work)),
		work.join(".git"),
	)
}

fn pending_on_read(
	work: &Path,
	path: &str,
	pend_at: usize,
) -> WorkTree<PendingReadStore, CapWorkDir, Sha256> {
	let store = PendingReadStore {
		inner: LocalFileStore::from_dir(open_dir(work.join(".git"))),
		path: path.to_owned(),
		reads: Arc::new(AtomicUsize::new(0)),
		pend_at,
		on_lock: None,
	};
	WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(store)),
		CapWorkDir::from_dir(open_dir(work)),
		work.join(".git"),
	)
}

impl FileStore for PendingReadStore {
	type Shared = Self;

	fn shared_handle(&self) -> Self::Shared {
		Self {
			inner: self.inner.shared_handle(),
			path: self.path.clone(),
			reads: Arc::clone(&self.reads),
			pend_at: self.pend_at,
			on_lock: self.on_lock.clone(),
		}
	}

	fn durability_barrier(
		&self,
		targets: &[DurabilityTarget],
	) -> impl Future<Output = Result<()>> + Send {
		self.inner.durability_barrier(targets)
	}

	async fn read_path(&self, path: &str) -> Result<Vec<u8>> {
		if path == self.path && self.reads.fetch_add(1, Ordering::AcqRel) + 1 == self.pend_at {
			std::future::pending::<()>().await;
		}
		self.inner.read_path(path).await
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
		if let Some(hook) = &self.on_lock
			&& hook.path == path
			&& !hook.fired.swap(true, Ordering::AcqRel)
		{
			(hook.action)();
		}
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

	fn modified(
		&self,
		path: &str,
	) -> impl Future<Output = Result<Option<std::time::SystemTime>>> + Send {
		self.inner.modified(path)
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

/// How [`InterferingWorkDir`]'s other program changes `u1.txt`.
#[derive(Clone, Copy, Debug)]
enum Change {
	/// A new file renamed over it, as `u2.txt` is about to be created.
	Replace,
	/// Rewritten in place, the same file, as `u2.txt` is about to be created.
	Rewrite,
	/// A new file renamed over it the moment it's created, before anything else.
	SwapAtCreation,
	/// Rewritten in place with contents of the same length, its modification time put back, as
	/// `u2.txt` is about to be created.
	RewriteKeepingTime,
}

/// The local working tree, with another program at work: it changes `u1.txt` (see [`Change`]) and
/// takes the name `u2.txt` as that's about to be created, so the pop is refused after creating
/// `u1.txt`.
struct InterferingWorkDir {
	inner: CapWorkDir,
	root: PathBuf,
	change: Change,
}

impl InterferingWorkDir {
	/// As `path` is about to be created.
	fn before(&self, path: &GitPath) -> io::Result<()> {
		if path.as_bytes() == b"u2.txt" {
			let u1 = self.root.join("u1.txt");
			match self.change {
				Change::Replace => self.replace(&u1)?,
				Change::Rewrite => {
					let mut file = std::fs::OpenOptions::new().write(true).open(&u1)?;
					std::io::Write::write_all(&mut file, b"rewritten in place")?;
				}
				Change::SwapAtCreation => {}
				Change::RewriteKeepingTime => {
					let modified = std::fs::metadata(&u1)?.modified()?;
					let file = std::fs::OpenOptions::new().write(true).open(&u1)?;
					std::io::Write::write_all(&mut &file, b"ONE")?;
					file.set_modified(modified)?;
				}
			}
			std::fs::write(self.root.join("u2.txt"), b"theirs")?;
		}
		Ok(())
	}

	/// Just after `path` was created.
	fn after(&self, path: &GitPath) -> io::Result<()> {
		if path.as_bytes() == b"u1.txt" && matches!(self.change, Change::SwapAtCreation) {
			self.replace(&self.root.join("u1.txt"))?;
		}
		Ok(())
	}

	fn replace(&self, target: &Path) -> io::Result<()> {
		std::fs::write(self.root.join("prepared"), b"replaced")?;
		std::fs::rename(self.root.join("prepared"), target)
	}
}

impl WorkDirFs for InterferingWorkDir {
	fn uses_symlink_placeholders(&self) -> bool {
		self.inner.uses_symlink_placeholders()
	}

	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()> {
		self.inner.validate_path_representable(path)
	}

	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>> {
		self.inner.lstat(path)
	}

	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.inner.read(path)
	}

	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.inner.read_link(path)
	}

	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>> {
		self.inner.read_dir(path)
	}

	fn write(&self, path: &GitPath, bytes: &[u8], executable: bool) -> io::Result<()> {
		self.inner.write(path, bytes, executable)
	}

	fn populate_new(&self, path: &GitPath, entry: PopulationEntry<'_>) -> io::Result<bool> {
		self.before(path)?;
		let made = self.inner.populate_new(path, entry)?;
		self.after(path)?;
		Ok(made)
	}

	fn populate_new_recorded(
		&self,
		path: &GitPath,
		entry: PopulationEntry<'_>,
	) -> io::Result<Option<WorkEntryIdentity>> {
		self.before(path)?;
		let recorded = self.inner.populate_new_recorded(path, entry)?;
		self.after(path)?;
		Ok(recorded)
	}

	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()> {
		self.inner.symlink(target, path)
	}

	fn create_dir(&self, path: &GitPath) -> io::Result<()> {
		self.inner.create_dir(path)
	}

	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()> {
		self.inner.rename(from, to)
	}

	fn remove_file(&self, path: &GitPath) -> io::Result<()> {
		self.inner.remove_file(path)
	}

	fn remove_dir(&self, path: &GitPath) -> io::Result<()> {
		self.inner.remove_dir(path)
	}

	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()> {
		self.inner.remove_dir_all(path)
	}

	fn entry_identity(&self, path: &GitPath) -> io::Result<Option<WorkEntryIdentity>> {
		self.inner.entry_identity(path)
	}

	fn remove_file_if_identity(
		&self,
		path: &GitPath,
		expected: &WorkEntryIdentity,
	) -> io::Result<bool> {
		self.inner.remove_file_if_identity(path, expected)
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
