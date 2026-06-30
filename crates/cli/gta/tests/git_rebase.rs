//! `gta rebase` end-to-end: replaying a branch's commits onto a new base, with the
//! `--continue` / `--skip` / `--abort` conflict lifecycle. Cross-checked against real git where
//! deterministic — the rebased tree matches stock `git rebase`, and git agrees a conflicted index is
//! `UU`.

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn linear_rebase_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-rebase-linear");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "x.txt", "x\n");
	commit_all(w, "B");
	write(&work, "y.txt", "y\n");
	let c = commit_all(w, "C");
	git(w, &["branch", "feature2", &c]); // a copy for the git oracle
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let m = commit_all(w, "M");

	git(w, &["checkout", "-q", "feature"]);
	gta(w, &["rebase", &main], b"");

	// Replayed onto M: three commits up from M, all four files present, original tip gone.
	assert_eq!(gta(w, &["rev-parse", "HEAD~2"], b"").trim(), m);
	assert_ne!(gta(w, &["rev-parse", "HEAD"], b"").trim(), c);
	for f in ["base.txt", "x.txt", "y.txt", "m.txt"] {
		assert!(work.join(f).exists(), "{f} present after rebase");
	}
	assert!(gta(w, &["status"], b"").is_empty(), "clean after rebase");
	let gta_tree = gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim().to_owned();

	// Oracle: stock git rebases the same commits onto main; the final trees must match.
	git(w, &["checkout", "-q", "feature2"]);
	git(w, &["rebase", &main]);
	assert_eq!(gta_tree, git(w, &["rev-parse", "HEAD^{tree}"]).trim());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn conflict_then_continue() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-continue");
	let w = work.to_str().unwrap();
	let main = head_branch(w);
	let (_, _) = setup_conflict(&work, w);

	let (stdout, _) = gta_out_fail(w, &["rebase", &main]);
	assert!(stdout.contains("CONFLICT"), "conflict reported: {stdout}");
	assert!(work.join(".git/REBASE_TODO").exists());
	assert!(gta(w, &["status"], b"").contains("UU f.txt"));
	assert!(git(w, &["status", "--porcelain"]).contains("UU f.txt"));

	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");
	gta(w, &["rebase", "--continue"], b"");

	assert!(
		!work.join(".git/REBASE_HEAD_NAME").exists(),
		"state cleared"
	);
	assert!(gta(w, &["status"], b"").is_empty());
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"resolved\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn abort_restores_original_branch() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-abort");
	let w = work.to_str().unwrap();
	let main = head_branch(w);
	let (orig, _) = setup_conflict(&work, w);

	gta_out_fail(w, &["rebase", &main]);
	gta(w, &["rebase", "--abort"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), orig); // branch restored
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());
	assert!(gta(w, &["status"], b"").is_empty(), "clean after abort");
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"feature\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn skip_drops_the_conflicting_commit() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-skip");
	let w = work.to_str().unwrap();
	let main = head_branch(w);
	let (_, m) = setup_conflict(&work, w);

	gta_out_fail(w, &["rebase", &main]);
	gta(w, &["rebase", "--skip"], b"");

	// The only commit conflicted and was skipped, so the branch is left at the base (M).
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), m);
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());
	assert!(gta(w, &["status"], b"").is_empty());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn already_applied_commit_is_dropped() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-empty");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "b.txt", "b\n");
	commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);
	write(&work, "b.txt", "b\n"); // same change lands on main independently
	let m = commit_all(w, "M");

	git(w, &["checkout", "-q", "feature"]);
	gta(w, &["rebase", &main], b"");

	// B's change is already in M, so it is dropped: the branch ends exactly at M.
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), m);
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn up_to_date_and_fast_forward() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-ff");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]); // feature == main
	let out = gta(w, &["rebase", &main], b"");
	assert!(out.contains("up to date"), "{out}");

	// Advance main; feature is behind -> rebase fast-forwards it.
	git(w, &["checkout", "-q", &main]);
	write(&work, "b.txt", "b\n");
	let m = commit_all(w, "M");
	git(w, &["checkout", "-q", "feature"]);
	let out = gta(w, &["rebase", &main], b"");
	assert!(out.contains("Fast-forwarded"), "{out}");
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), m);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn onto_rebases_onto_a_different_base() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-onto");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "topic"]);
	write(&work, "t.txt", "t\n");
	commit_all(w, "T");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "f\n");
	let f = commit_all(w, "F");
	git(w, &["branch", "feature2", &f]);
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let m = commit_all(w, "M");

	// Replay topic..feature (just F) onto main, dropping T.
	git(w, &["checkout", "-q", "feature"]);
	gta(w, &["rebase", "--onto", &main, "topic"], b"");
	assert_eq!(gta(w, &["rev-parse", "HEAD~1"], b"").trim(), m);
	assert!(work.join("f.txt").exists() && !work.join("t.txt").exists());
	let gta_tree = gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim().to_owned();

	git(w, &["checkout", "-q", "feature2"]);
	git(w, &["rebase", "--onto", &main, "topic"]);
	assert_eq!(gta_tree, git(w, &["rev-parse", "HEAD^{tree}"]).trim());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn refuses_dirty_index_and_merge_in_range() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-refuse");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "f\n");
	commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	commit_all(w, "M");
	git(w, &["checkout", "-q", "feature"]);

	// Dirty index → refused, staged change preserved.
	write(&work, "staged.txt", "s\n");
	gta(w, &["add", "staged.txt"], b"");
	gta_fail(w, &["rebase", &main]);
	assert!(work.join("staged.txt").exists());
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());
	gta(w, &["rm", "--cached", "staged.txt"], b"");

	// A merge commit in the range → refused.
	git(w, &["checkout", "-q", "-b", "side", "feature"]);
	write(&work, "side.txt", "s\n");
	commit_all(w, "S");
	git(w, &["checkout", "-q", "feature"]);
	git(w, &["merge", "--no-ff", "--no-edit", "side"]);
	gta_fail(w, &["rebase", &main]);
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn refuses_unstaged_tracked_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-unstaged");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "f.txt", "f\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "g.txt", "g\n");
	let orig = commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	commit_all(w, "M");
	git(w, &["checkout", "-q", "feature"]);

	// An unstaged edit to a tracked file: git refuses a rebase, and so must gta.
	write(&work, "f.txt", "dirty\n");
	gta_fail(w, &["rebase", &main]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), orig); // branch untouched
	assert!(!work.join(".git/REBASE_HEAD_NAME").exists());
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"dirty\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn failed_start_leaves_no_rebase_state() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-failed-start");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "f\n");
	let orig = commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	commit_all(w, "M"); // main adds m.txt
	git(w, &["checkout", "-q", "feature"]);

	// An untracked m.txt collides with the checkout to the base: the rebase must fail *without*
	// leaving phantom state behind (git refuses before entering a rebase).
	write(&work, "m.txt", "untracked\n");
	gta_fail(w, &["rebase", &main]);
	assert!(
		!work.join(".git/REBASE_HEAD_NAME").exists(),
		"no phantom rebase state after a failed start"
	);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), orig); // branch not moved
	// A later history operation is not blocked.
	gta(w, &["rev-parse", "HEAD"], b"");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn originally_empty_commit_is_preserved() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rebase-keep-empty");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	git(w, &["commit", "-q", "--allow-empty", "-m", "EMPTY"]); // empty in the original history
	write(&work, "x.txt", "x\n");
	let f = commit_all(w, "F");
	git(w, &["branch", "feature2", &f]);
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let m = commit_all(w, "M");

	git(w, &["checkout", "-q", "feature"]);
	gta(w, &["rebase", &main], b"");

	// git keeps the originally-empty commit: the branch is M <- EMPTY' <- F', so HEAD~2 is M and the
	// kept commit is empty (its tree equals its parent's).
	assert_eq!(gta(w, &["rev-parse", "HEAD~2"], b"").trim(), m);
	assert_eq!(
		gta(w, &["rev-parse", "HEAD~1^{tree}"], b"").trim(),
		gta(w, &["rev-parse", "HEAD~2^{tree}"], b"").trim(),
		"the preserved commit is empty"
	);
	// Oracle: stock git's rebase keeps it too.
	git(w, &["checkout", "-q", "feature2"]);
	git(w, &["rebase", &main]);
	assert_eq!(git(w, &["rev-parse", "HEAD~2"]).trim(), m);

	std::fs::remove_dir_all(&work).ok();
}

/// Build a one-commit rebase conflict: base `f.txt`, feature sets it to "feature", main sets it to
/// "main". Leaves `feature` checked out. Returns `(feature_orig_tip, main_tip)`.
fn setup_conflict(work: &Path, w: &str) -> (String, String) {
	let main = head_branch(w);
	write(work, "f.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(work, "f.txt", "feature\n");
	let orig = commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);
	write(work, "f.txt", "main\n");
	let m = commit_all(w, "M");
	git(w, &["checkout", "-q", "feature"]);
	(orig, m)
}

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	work
}

fn head_branch(w: &str) -> String {
	git(w, &["symbolic-ref", "--short", "HEAD"])
		.trim()
		.to_owned()
}

fn write(work: &Path, name: &str, content: &str) {
	std::fs::write(work.join(name), content).unwrap();
}

fn commit_all(w: &str, msg: &str) -> String {
	git(w, &["add", "."]);
	git(w, &["commit", "-q", "-m", msg]);
	git(w, &["rev-parse", "HEAD"]).trim().to_owned()
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

fn gta_out_fail(dir: &str, args: &[&str]) -> (String, String) {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(!out.status.success(), "gta {args:?} unexpectedly succeeded");
	(
		String::from_utf8(out.stdout).expect("gta stdout utf8"),
		String::from_utf8(out.stderr).expect("gta stderr utf8"),
	)
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
		let probe = unique_tmp("probe-rebase");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
