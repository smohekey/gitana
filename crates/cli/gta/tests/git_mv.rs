//! `gta mv` end-to-end: renames and directory moves update both the working tree and the index,
//! enforce git's destination/source checks with `-f`, and support `--dry-run` — cross-checked
//! against real git's view of the result.

use std::path::PathBuf;
use std::process::Command;

/// A repo with committed `a.txt`=A, `c.txt`=C, and `dir/x.txt`=X. Returns the work dir.
fn repo_with_files(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	std::fs::write(work.join("c.txt"), b"C\n").unwrap();
	std::fs::create_dir(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/x.txt"), b"X\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	work
}

#[test]
fn mv_renames_a_tracked_file() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = repo_with_files("gta-mv-rename");
	let w = work.to_str().unwrap();

	gta(w, &["mv", "a.txt", "b.txt"], b"");

	assert!(!work.join("a.txt").exists());
	assert_eq!(std::fs::read(work.join("b.txt")).unwrap(), b"A\n");
	let tracked = git(w, &["ls-files"]);
	assert!(tracked.contains("b.txt") && !tracked.contains("a.txt"));
	// The staged change is a rename (same blob), as git reports it.
	assert!(
		git(w, &["status", "--porcelain"]).contains("R  a.txt -> b.txt"),
		"status: {}",
		git(w, &["status", "--porcelain"])
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_into_existing_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-into-dir");
	let w = work.to_str().unwrap();
	std::fs::create_dir(work.join("sub")).unwrap();

	gta(w, &["mv", "a.txt", "c.txt", "sub"], b"");

	assert!(!work.join("a.txt").exists() && !work.join("c.txt").exists());
	assert_eq!(std::fs::read(work.join("sub/a.txt")).unwrap(), b"A\n");
	assert_eq!(std::fs::read(work.join("sub/c.txt")).unwrap(), b"C\n");
	let tracked = git(w, &["ls-files"]);
	assert!(tracked.contains("sub/a.txt") && tracked.contains("sub/c.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_renames_a_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-dir");
	let w = work.to_str().unwrap();

	gta(w, &["mv", "dir", "newdir"], b"");

	assert!(!work.join("dir").exists());
	assert_eq!(std::fs::read(work.join("newdir/x.txt")).unwrap(), b"X\n");
	let tracked = git(w, &["ls-files"]);
	assert!(
		tracked.lines().any(|l| l == "newdir/x.txt") && !tracked.lines().any(|l| l == "dir/x.txt"),
		"ls-files: {tracked}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_refuses_existing_destination_without_force() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-exists");
	let w = work.to_str().unwrap();

	// `c.txt` already exists — refuse without -f, leaving both files intact.
	let err = gta_fail(w, &["mv", "a.txt", "c.txt"]);
	assert!(err.contains("already exists"), "stderr: {err}");
	assert!(work.join("a.txt").exists());
	assert_eq!(std::fs::read(work.join("c.txt")).unwrap(), b"C\n");

	// With -f the destination is overwritten.
	gta(w, &["mv", "-f", "a.txt", "c.txt"], b"");
	assert!(!work.join("a.txt").exists());
	assert_eq!(std::fs::read(work.join("c.txt")).unwrap(), b"A\n");
	assert!(!git(w, &["ls-files"]).contains("a.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_refuses_untracked_source() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-untracked");
	let w = work.to_str().unwrap();
	std::fs::write(work.join("u.txt"), b"U\n").unwrap();

	let err = gta_fail(w, &["mv", "u.txt", "v.txt"]);
	assert!(err.contains("not under version control"), "stderr: {err}");
	assert!(work.join("u.txt").exists() && !work.join("v.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_into_itself_errors() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-self");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["mv", "dir", "dir/sub"]);
	assert!(err.contains("into itself"), "stderr: {err}");
	assert!(git(w, &["ls-files"]).contains("dir/x.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_dry_run_changes_nothing() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-dryrun");
	let w = work.to_str().unwrap();

	let out = gta(w, &["mv", "-n", "a.txt", "b.txt"], b"");
	assert_eq!(out, "Renaming a.txt to b.txt\n");
	// Reported, but nothing moved.
	assert!(work.join("a.txt").exists() && !work.join("b.txt").exists());
	assert!(git(w, &["ls-files"]).contains("a.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_preserves_an_unstaged_modification() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-dirty");
	let w = work.to_str().unwrap();

	// Modify the tracked file so the working tree differs from the staged blob, then move it.
	std::fs::write(work.join("a.txt"), b"DIRTY\n").unwrap();
	gta(w, &["mv", "a.txt", "b.txt"], b"");

	// The dirty content travels to b.txt, and the modification stays unstaged — `gta status`
	// must not hide it behind a refreshed stat cache.
	assert_eq!(std::fs::read(work.join("b.txt")).unwrap(), b"DIRTY\n");
	let status = gta(w, &["status"], b"");
	assert!(
		status.contains("M b.txt"),
		"gta status should report b.txt as modified: {status}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn mv_releases_index_lock_on_failure() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-mv-lockfail");
	let w = work.to_str().unwrap();
	// A directory target where the second source's destination is an existing non-empty
	// directory, so renaming a file onto it fails after the first source has moved.
	std::fs::create_dir(work.join("target")).unwrap();
	std::fs::create_dir(work.join("target/c.txt")).unwrap();
	std::fs::write(work.join("target/c.txt/inner"), b"x\n").unwrap();

	let err = gta_fail(w, &["mv", "-f", "a.txt", "c.txt", "target"]);
	assert!(!err.is_empty());
	// The fix: the index lock is released rather than left behind for the next command.
	assert!(
		!work.join(".git/index.lock").exists(),
		"index.lock must be released on a mid-move failure"
	);

	std::fs::remove_dir_all(&work).ok();
}

fn commit(dir: &str, msg: &str) {
	git(
		dir,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			msg,
		],
	);
}

fn gta(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.write_stdin(stdin.to_vec())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}

fn gta_fail(dir: &str, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(!out.status.success(), "gta {args:?} unexpectedly succeeded");
	String::from_utf8(out.stderr).expect("gta stderr utf8")
}

fn git(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
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
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-mv");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
