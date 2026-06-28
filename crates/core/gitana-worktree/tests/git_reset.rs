#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::ObjectId;
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use gitana_worktree::{WorkTree, WorktreeError};

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	WorkTree::new(repo, work, git_dir)
}

#[tokio::test]
async fn reset_index_replaces_index_with_tree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("reset-index");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Commit one: only `a.txt`.
	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let first = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Commit two: edit `a.txt` and add `b.txt`; the index now matches commit two.
	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "two");

	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&first).unwrap())
		.await
		.unwrap();

	wt.reset_index(tree1).await.unwrap();

	// The index now matches commit one (only `a.txt`=A1), while the working tree is untouched —
	// so `b.txt` is untracked and `a.txt` reads as an unstaged modification.
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "a.txt");
	let staged = git(&["-C", w, "diff", "--cached", "--name-only", &first]);
	assert!(staged.is_empty(), "index equals commit one: {staged:?}");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");
	assert_eq!(std::fs::read(work.join("b.txt")).unwrap(), b"B\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn reset_index_rejects_unsafe_tree_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("reset-index-unsafe");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let wt = make_repo(&work);
	let before = wt.load_index().unwrap();

	// A crafted tree whose flattened entry escapes the work tree must not enter the index.
	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	let hostile = wt
		.repository()
		.write_tree(&[TreeBuildEntry {
			path: "../escape.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();

	assert!(matches!(
		wt.reset_index(hostile).await,
		Err(WorktreeError::UnsafePath(_))
	));
	assert_eq!(wt.load_index().unwrap(), before, "the index is untouched");

	std::fs::remove_dir_all(&work).ok();
}

fn commit(work: &str, msg: &str) {
	git(&[
		"-C",
		work,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		msg,
	]);
}

fn git(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-worktree-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-reset");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
