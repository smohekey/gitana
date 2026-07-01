#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

#[tokio::test]
async fn add_stages_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("add");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("README.md"), b"readme\n").unwrap();
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/lib.rs"), b"lib\n").unwrap();
	let script = work.join("run.sh");
	std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
	std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
	std::os::unix::fs::symlink("README.md", work.join("link")).unwrap();

	let paths = ["README.md", "src/lib.rs", "run.sh", "link"];

	// Stage with our worktree.
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, &work, &git_dir)
		.add(&paths, "")
		.await
		.unwrap();
	let ours = ls_files(w);

	// Stage the same paths with git from an empty index.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	let mut args = vec!["-C", w, "add"];
	args.extend_from_slice(&paths);
	git(&args);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs, "our add must stage identically to git add");
	// Sanity: exec bit and symlink modes are present.
	assert!(
		ours
			.iter()
			.any(|l| l.starts_with("100755 ") && l.ends_with("\trun.sh"))
	);
	assert!(
		ours
			.iter()
			.any(|l| l.starts_with("120000 ") && l.ends_with("\tlink"))
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_with_prefix_is_relative_to_subdirectory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-prefix");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"ROOT\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/a.txt"), b"SUB\n").unwrap();

	// From the `sub` directory, `a.txt` means `sub/a.txt`, like `git -C sub add a.txt`.
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, &work, &git_dir)
		.add(&["a.txt"], "sub")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", &format!("{w}/sub"), "add", "a.txt"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs);
	assert!(ours.iter().any(|l| l.ends_with("\tsub/a.txt")));
	assert!(
		!ours
			.iter()
			.any(|l| l.ends_with("\ta.txt") && !l.ends_with("\tsub/a.txt"))
	);

	// From `sub`, `../a.txt` resolves to the root file and is stored as `a.txt`, not
	// `sub/../a.txt`, matching `git -C sub add ../a.txt`.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, &work, &git_dir)
		.add(&["../a.txt"], "sub")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", &format!("{w}/sub"), "add", "../a.txt"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs);
	assert!(
		ours
			.iter()
			.any(|l| l.ends_with("\ta.txt") && !l.ends_with("\tsub/a.txt"))
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_trailing_slash_requires_a_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-trailing-slash");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"v1\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/x.txt"), b"s1\n").unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, &work, &git_dir);

	// `a.txt/` and `a.txt/.` name a file as a directory: git rejects them, and so do we.
	for spec in ["a.txt/", "a.txt/."] {
		assert!(matches!(
			wt.add(&[spec], "").await,
			Err(gitana_worktree::WorktreeError::PathspecMatch(_))
		));
		let git_ok = Command::new("git")
			.args(["-C", w, "add", spec])
			.output()
			.unwrap()
			.status
			.success();
		assert!(!git_ok, "git also rejects '{spec}'");
	}
	assert!(ls_files(w).is_empty(), "nothing was staged");

	// A trailing slash on an actual directory still works.
	wt.add(&["sub/"], "").await.unwrap();
	assert!(ls_files(w).iter().any(|l| l.ends_with("\tsub/x.txt")));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_rewrites_index_on_file_directory_type_change() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-typechange");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, &work, &git_dir);

	// Stage `thing` as a file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1);
	assert!(files[0].ends_with("\tthing"));

	// Replace it with a directory and re-add: the stale file entry must be dropped.
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1, "no stale `thing` file entry remains");
	assert!(files[0].ends_with("\tthing/child.txt"));

	// And the reverse: directory back to a file drops the `thing/child.txt` entry.
	std::fs::remove_dir_all(work.join("thing")).unwrap();
	std::fs::write(work.join("thing"), b"FILE2\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1);
	assert!(files[0].ends_with("\tthing"));

	std::fs::remove_dir_all(&work).ok();
}

fn ls_files(work: &str) -> Vec<String> {
	git(&["-C", work, "ls-files", "--stage"])
		.lines()
		.map(str::to_owned)
		.collect()
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
	// A per-call sequence number keeps every temp dir distinct even for the same tag, so
	// tests running in parallel threads never race on a shared path.
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!(
		"gitana-worktree-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	// Probe once per test binary: every test calls this, and a shared probe dir raced under
	// load. `OnceLock` makes it concurrency-safe and spawns `git init` a single time.
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-add");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
