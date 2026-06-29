//! `gta` behavior on a conflicted (unmerged) index, cross-checked against real git: a `git merge`
//! conflict must produce the same `gta status` porcelain codes (`UU`/`AA`/`UD`), `gta add` and
//! `gta rm` must resolve it the way git sees it, and `gta commit` must refuse while conflicts remain.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `gta status` and `git status --porcelain` must agree.
fn assert_status_matches(w: &str) {
	let gta_out = gta(w, &["status"], b"");
	let git_out = git(w, &["status", "--porcelain"]);
	assert_eq!(
		sorted_lines(&gta_out),
		sorted_lines(&git_out),
		"gta:\n{gta_out}\n---\ngit:\n{git_out}"
	);
}

#[test]
fn both_modified_is_uu_and_resolves() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-status-uu");
	let w = work.to_str().unwrap();

	// base, then two branches editing the same line differently → a `UU` conflict on merge.
	write(&work, "f.txt", "base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "f.txt", "OURS\n");
	commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "f.txt", "THEIRS\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", "ours"]);
	merge_expecting_conflict(w, "theirs");

	assert_eq!(gta(w, &["status"], b"").trim(), "UU f.txt");
	assert_status_matches(w);

	// Staging the conflicted path resolves it; git and gta agree on the result.
	gta(w, &["add", "f.txt"], b"");
	assert!(!gta(w, &["status"], b"").contains("UU"));
	assert_status_matches(w);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn both_added_is_aa() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-status-aa");
	let w = work.to_str().unwrap();

	write(&work, "base.txt", "base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "new.txt", "ours\n");
	commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "new.txt", "theirs\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", "ours"]);
	merge_expecting_conflict(w, "theirs");

	assert_eq!(gta(w, &["status"], b"").trim(), "AA new.txt");
	assert_status_matches(w);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn modify_delete_is_ud() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-status-ud");
	let w = work.to_str().unwrap();

	write(&work, "f.txt", "base\n");
	let base = commit_all(w, "base");
	// ours modifies f.txt; theirs deletes it → "deleted by them" (UD) when merging theirs into ours.
	checkout_new(w, "ours", &base);
	write(&work, "f.txt", "OURS\n");
	commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	git(w, &["rm", "-q", "f.txt"]);
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", "ours"]);
	merge_expecting_conflict(w, "theirs");

	assert_eq!(gta(w, &["status"], b"").trim(), "UD f.txt");
	assert_status_matches(w);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn commit_with_unmerged_files_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-commit-unmerged");
	let w = work.to_str().unwrap();

	write(&work, "keep.txt", "keep\n");
	write(&work, "f.txt", "base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "f.txt", "OURS\n");
	commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "f.txt", "THEIRS\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", "ours"]);
	merge_expecting_conflict(w, "theirs");

	let head_before = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	let err = gta_fail(w, &["commit", "-m", "bad"]);
	assert!(err.contains("unmerged"), "stderr: {err}");
	// HEAD did not move and the conflict is still present.
	assert_eq!(git(w, &["rev-parse", "HEAD"]).trim(), head_before);
	assert!(gta(w, &["status"], b"").contains("UU f.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_resolves_a_conflict_by_deletion() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-rm-unmerged");
	let w = work.to_str().unwrap();

	write(&work, "keep.txt", "keep\n");
	write(&work, "f.txt", "base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "f.txt", "OURS\n");
	commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "f.txt", "THEIRS\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", "ours"]);
	merge_expecting_conflict(w, "theirs");

	assert_eq!(gta(w, &["status"], b"").trim(), "UU f.txt");

	// `gta rm` resolves the conflict by deleting the path — exactly as `git rm` does.
	gta(w, &["rm", "f.txt"], b"");
	assert!(!work.join("f.txt").exists());
	assert_eq!(gta(w, &["status"], b"").trim(), "D  f.txt");
	assert_status_matches(w);

	// With the conflict resolved, committing is allowed again.
	gta(w, &["commit", "-m", "resolve by deleting f.txt"], b"");

	std::fs::remove_dir_all(&work).ok();
}

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	// Persist an identity so `gta commit` (a separate process) can read user.name/email.
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	work
}

fn write(work: &Path, name: &str, content: &str) {
	std::fs::write(work.join(name), content).unwrap();
}

fn commit_all(w: &str, msg: &str) -> String {
	git(w, &["add", "."]);
	git_id(w, &["commit", "-q", "-m", msg]);
	git(w, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn checkout_new(w: &str, branch: &str, start: &str) {
	git(w, &["checkout", "-q", "-b", branch, start]);
}

/// `git merge <branch>` that is expected to conflict (exit non-zero).
fn merge_expecting_conflict(w: &str, branch: &str) {
	let ok = Command::new("git")
		.args([
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"merge",
			branch,
		])
		.output()
		.expect("run git merge")
		.status
		.success();
	assert!(!ok, "git merge {branch} was expected to conflict");
}

fn sorted_lines(text: &str) -> Vec<String> {
	let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
	lines.sort();
	lines
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

fn git_id(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir, "-c", "user.name=T", "-c", "user.email=t@e"];
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
		let probe = unique_tmp("probe-status-conflict");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
