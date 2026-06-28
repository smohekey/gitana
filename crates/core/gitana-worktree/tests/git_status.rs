#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

#[tokio::test]
async fn status_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	std::fs::write(work.join("b.txt"), b"2\n").unwrap();
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/c.txt"), b"3\n").unwrap();
	git(&["-C", w, "add", "."]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	// Unstaged modification, staged modification, unstaged deletion, untracked.
	std::fs::write(work.join("a.txt"), b"1 changed\n").unwrap();
	std::fs::write(work.join("b.txt"), b"2 new\n").unwrap();
	git(&["-C", w, "add", "b.txt"]);
	std::fs::remove_file(work.join("dir/c.txt")).unwrap();
	std::fs::write(work.join("new.txt"), b"x\n").unwrap();

	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	let ours = sorted(
		&WorkTree::new(repo, &work, &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));

	assert_eq!(ours, theirs, "our status must match git status --porcelain");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_with_gitignore_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ignore");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	std::fs::write(work.join(".gitignore"), b"*.log\nbuild/\n").unwrap();
	git(&["-C", w, "add", "a.txt", ".gitignore"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	// Ignored (omitted), untracked directory (collapsed), untracked file, modified.
	std::fs::write(work.join("debug.log"), b"log\n").unwrap();
	std::fs::create_dir_all(work.join("build")).unwrap();
	std::fs::write(work.join("build/out.o"), b"obj\n").unwrap();
	std::fs::create_dir_all(work.join("newdir")).unwrap();
	std::fs::write(work.join("newdir/x.txt"), b"x\n").unwrap();
	std::fs::write(work.join("keep.txt"), b"k\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a changed\n").unwrap();

	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	let ours = sorted(
		&WorkTree::new(repo, &work, &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));

	assert_eq!(
		ours, theirs,
		"status must match git with .gitignore + untracked dir"
	);
	std::fs::remove_dir_all(&work).ok();
}

fn sorted(porcelain: &str) -> Vec<String> {
	let mut lines: Vec<String> = porcelain.lines().map(str::to_owned).collect();
	lines.sort();
	lines
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
	let dir = std::env::temp_dir().join(format!("gitana-worktree-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	let probe = unique_tmp("probe-status");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
