//! `gta reset` end-to-end: soft/mixed/hard whole-tree resets move the branch and reset the
//! index/working tree as git does, path resets touch only the index, and detached HEAD and the
//! reflog are handled — all cross-checked against real git.

use std::path::PathBuf;
use std::process::Command;

/// Two commits on `main`: one (`a.txt`=A1), then two (`a.txt`=A2, `b.txt`=B). Returns the work
/// dir and the first commit id. HEAD is left at the second commit.
fn two_commits(tag: &str) -> (PathBuf, String) {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");

	(work, c1)
}

#[test]
fn reset_soft_moves_head_only() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let (work, c1) = two_commits("gta-reset-soft");
	let w = work.to_str().unwrap();

	gta(w, &["reset", "--soft", "HEAD~1"], b"");

	// HEAD moved to the first commit, but the index and working tree still hold commit two.
	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), c1);
	assert_eq!(porcelain(w), "A  b.txt\nM  a.txt");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_mixed_moves_head_and_resets_index() {
	if !git_supports_sha256() {
		return;
	}
	let (work, c1) = two_commits("gta-reset-mixed");
	let w = work.to_str().unwrap();

	// `--mixed` is the default.
	gta(w, &["reset", "HEAD~1"], b"");

	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), c1);
	// Index reset to the first commit; the working tree still has commit two's content, so the
	// edit is unstaged and `b.txt` is now untracked.
	assert_eq!(porcelain(w), " M a.txt\n?? b.txt");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_hard_resets_index_and_worktree() {
	if !git_supports_sha256() {
		return;
	}
	let (work, c1) = two_commits("gta-reset-hard");
	let w = work.to_str().unwrap();

	gta(w, &["reset", "--hard", "HEAD~1"], b"");

	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), c1);
	// Index and working tree both back to the first commit: clean, `a.txt`=A1, no `b.txt`.
	assert!(porcelain(w).is_empty());
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert!(!work.join("b.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_path_resets_index_without_moving_head() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-reset-path");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// Stage a further edit, then reset just that path back to HEAD.
	std::fs::write(work.join("a.txt"), b"A3\n").unwrap();
	git(w, &["add", "a.txt"]);
	gta(w, &["reset", "--", "a.txt"], b"");

	// HEAD did not move; the index entry is back to HEAD while the working-tree edit remains.
	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), head);
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
	assert_eq!(porcelain(w), " M a.txt");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A3\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_path_unmatched_is_noop() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-reset-nomatch");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// An untracked file and a path that exists nowhere: both are no-op successes, as in git.
	std::fs::write(work.join("u.txt"), b"U\n").unwrap();
	gta(w, &["reset", "--", "u.txt"], b"");
	gta(w, &["reset", "--", "missing.txt"], b"");

	// Nothing changed: `u.txt` stays untracked and HEAD did not move.
	assert_eq!(porcelain(w), "?? u.txt");
	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), head);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_path_unmatched_on_unborn_branch_is_noop() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-reset-unborn-nomatch");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// Untracked, never staged, on a branch with no commits.
	std::fs::write(work.join("u.txt"), b"U\n").unwrap();
	gta(w, &["reset", "--", "u.txt"], b"");
	gta(w, &["reset", "--", "missing.txt"], b"");

	assert_eq!(porcelain(w), "?? u.txt");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_path_on_unborn_branch_unstages() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-reset-unborn");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// Stage a file before any commit exists (HEAD is an unborn branch).
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(w, &["add", "a.txt"]);
	assert_eq!(porcelain(w), "A  a.txt");

	// `reset -- a.txt` defaults to HEAD; with no commit, it unstages, leaving the file untracked.
	gta(w, &["reset", "--", "a.txt"], b"");
	assert_eq!(porcelain(w), "?? a.txt");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_records_orig_head() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-reset-orig");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	gta(w, &["reset", "--hard", "HEAD~1"], b"");

	// ORIG_HEAD records the pre-reset tip, so `reset ORIG_HEAD` can recover it.
	assert_eq!(git(w, &["rev-parse", "ORIG_HEAD"]).trim(), head);
	gta(w, &["reset", "--hard", "ORIG_HEAD"], b"");
	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), head);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_writes_reflog() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-reset-reflog");
	let w = work.to_str().unwrap();

	gta(w, &["reset", "--hard", "HEAD~1"], b"");

	// The branch and HEAD reflogs record the move, recoverable via `git reflog`.
	let subject = git(w, &["reflog", "-1", "--format=%gs"]);
	assert_eq!(subject.trim(), "reset: moving to HEAD~1");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_moves_detached_head() {
	if !git_supports_sha256() {
		return;
	}
	let (work, c1) = two_commits("gta-reset-detached");
	let w = work.to_str().unwrap();

	// Detach HEAD at commit two, then reset it back one commit.
	git(w, &["checkout", "--detach"]);
	gta(w, &["reset", "--hard", "HEAD~1"], b"");

	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), c1);
	// HEAD is still detached (not pointing at a branch).
	assert!(
		!Command::new("git")
			.args(["-C", w, "symbolic-ref", "HEAD"])
			.output()
			.unwrap()
			.status
			.success(),
		"HEAD remained detached"
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn reset_rejects_mode_flags_with_paths() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-reset-badargs");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["reset", "--hard", "--", "a.txt"]);
	assert!(err.contains("cannot be combined with paths"), "stderr: {err}");

	let err = gta_fail(w, &["reset", "--soft", "--mixed", "HEAD~1"]);
	assert!(err.contains("mutually exclusive"), "stderr: {err}");

	std::fs::remove_dir_all(&work).ok();
}

fn porcelain(w: &str) -> String {
	let mut lines: Vec<String> = git(w, &["status", "--porcelain"])
		.lines()
		.map(str::to_owned)
		.collect();
	lines.sort();
	lines.join("\n")
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
