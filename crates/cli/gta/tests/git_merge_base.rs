//! `gta merge-base` end-to-end: the best common ancestor(s) of commits, and `--is-ancestor`, all
//! cross-checked against real git on histories with forks and merge commits.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn merge_base_matches_git_across_a_merge() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-merge-base");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// root on main, then a feature branch; main and feature each advance, then merge into main and
	// each advances once more — so resolving (main, feature) must cross the merge's two parents.
	write_add_commit(w, &work, "base.txt", "base", "root");
	git_id(w, &["branch", "feature"]);
	write_add_commit(w, &work, "m.txt", "m1", "main1");
	let m1 = head(w);
	git_id(w, &["checkout", "feature"]);
	write_add_commit(w, &work, "f.txt", "f1", "feat1");
	let f1 = head(w);
	git_id(w, &["checkout", "main"]);
	git_id(w, &["merge", "--no-ff", "-m", "merge", "feature"]);
	let merge = head(w);
	write_add_commit(w, &work, "m.txt", "m2", "main2");
	let m2 = head(w);
	git_id(w, &["checkout", "feature"]);
	write_add_commit(w, &work, "f.txt", "f2", "feat2");
	let f2 = head(w);

	for (a, b) in [(&m1, &f1), (&m2, &f2), (&merge, &m1), (&merge, &f2)] {
		assert_eq!(
			gta(w, &["merge-base", a, b], b"").trim(),
			git(w, &["merge-base", a, b]).trim(),
			"merge-base {a} {b}"
		);
	}

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn is_ancestor_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-merge-base-anc");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	write_add_commit(w, &work, "base.txt", "base", "root");
	let root = head(w);
	git_id(w, &["branch", "feature"]);
	write_add_commit(w, &work, "m.txt", "m1", "main1");
	let main1 = head(w);
	git_id(w, &["checkout", "feature"]);
	write_add_commit(w, &work, "f.txt", "f1", "feat1");
	let feat1 = head(w);

	for (a, b) in [
		(&root, &main1),  // ancestor
		(&main1, &root),  // not (descendant vs ancestor)
		(&root, &root),   // equal counts as ancestor
		(&main1, &feat1), // diverged: neither is an ancestor
	] {
		assert_eq!(
			gta_ok(w, &["merge-base", "--is-ancestor", a, b]),
			git_ok(w, &["merge-base", "--is-ancestor", a, b]),
			"is-ancestor {a} {b}"
		);
	}

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_base_all_matches_git_on_criss_cross() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-merge-base-cross");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// Two commits a, b off the root, then two independent merges of both — a criss-cross whose
	// best common ancestors are {a, b} (the root is redundant).
	write_add_commit(w, &work, "base.txt", "base", "root");
	git_id(w, &["checkout", "-b", "abr"]);
	write_add_commit(w, &work, "x.txt", "x", "a");
	let a = head(w);
	git_id(w, &["checkout", "-b", "bbr", "main"]);
	write_add_commit(w, &work, "y.txt", "y", "b");
	let b = head(w);
	git_id(w, &["checkout", "-b", "m1br", &a]);
	git_id(w, &["merge", "--no-ff", "-m", "m1", "bbr"]);
	let m1 = head(w);
	git_id(w, &["checkout", "-b", "m2br", &a]);
	git_id(w, &["merge", "--no-ff", "-m", "m2", "bbr"]);
	let m2 = head(w);

	// Sanity: it really is a criss-cross with two bases.
	let mut expected = [a.clone(), b.clone()];
	expected.sort();
	assert_eq!(
		sorted_lines(&git(w, &["merge-base", "--all", &m1, &m2])),
		expected
	);

	assert_eq!(
		sorted_lines(&gta(w, &["merge-base", "--all", &m1, &m2], b"")),
		sorted_lines(&git(w, &["merge-base", "--all", &m1, &m2])),
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_base_multi_arg_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-merge-base-multi");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// A <- C on main, plus an unrelated root B on an orphan branch.
	write_add_commit(w, &work, "base.txt", "base", "A");
	let a = head(w);
	write_add_commit(w, &work, "c.txt", "c", "C");
	let c = head(w);
	git_id(w, &["checkout", "--orphan", "ob"]);
	git(w, &["rm", "-rf", "."]);
	write_add_commit(w, &work, "o.txt", "o", "B");
	let b = head(w);

	// git's default multi-arg semantics: a base must descend from the first commit and reach at
	// least one of the rest — it need not be common to all. `merge-base C B A` is `A`.
	for args in [[&c, &b, &a], [&c, &a, &b]] {
		let argv = [
			"merge-base",
			args[0].as_str(),
			args[1].as_str(),
			args[2].as_str(),
		];
		assert_eq!(gta(w, &argv, b"").trim(), git(w, &argv).trim(), "{argv:?}");
	}
	// With the unrelated root first, there is no base (both exit non-zero).
	let no_base = ["merge-base", b.as_str(), c.as_str(), a.as_str()];
	assert_eq!(gta_ok(w, &no_base), git_ok(w, &no_base));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_base_default_matches_git_on_dated_criss_cross() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-merge-base-dated");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// A criss-cross whose two bases (`a`, `b`) have *distinct* committer dates, so git's single-base
	// choice is well defined (the newest, `b`) — letting us check default-mode parity, not just the
	// sorted `--all` set. (With equal dates git's pick among same-date bases is unspecified.)
	write_dated_commit(w, &work, "base.txt", "base", "root", "2001-01-01T00:00:00");
	git_id(w, &["checkout", "-b", "abr"]);
	write_dated_commit(w, &work, "x.txt", "x", "a", "2002-01-01T00:00:00");
	git_id(w, &["checkout", "-b", "bbr", "main"]);
	write_dated_commit(w, &work, "y.txt", "y", "b", "2003-01-01T00:00:00");
	let b_newer = head(w);
	git_id(w, &["checkout", "-b", "m1br", "abr"]);
	git_at(
		w,
		"2004-01-01T00:00:00",
		&["merge", "--no-ff", "-m", "m1", "bbr"],
	);
	let m1 = head(w);
	git_id(w, &["checkout", "-b", "m2br", "abr"]);
	git_at(
		w,
		"2005-01-01T00:00:00",
		&["merge", "--no-ff", "-m", "m2", "bbr"],
	);
	let m2 = head(w);

	// Default mode: gta and git agree, and both pick the newer base.
	let argv = ["merge-base", m1.as_str(), m2.as_str()];
	assert_eq!(gta(w, &argv, b"").trim(), git(w, &argv).trim());
	assert_eq!(gta(w, &argv, b"").trim(), b_newer);
	// --all order (newest first) matches exactly, without sorting either side.
	let allv = ["merge-base", "--all", m1.as_str(), m2.as_str()];
	assert_eq!(gta(w, &allv, b""), git(w, &allv));

	std::fs::remove_dir_all(&work).ok();
}

fn write_add_commit(w: &str, work: &std::path::Path, file: &str, content: &str, msg: &str) {
	std::fs::write(work.join(file), format!("{content}\n")).unwrap();
	git(w, &["add", "."]);
	git_id(w, &["commit", "-q", "-m", msg]);
}

fn write_dated_commit(
	w: &str,
	work: &std::path::Path,
	file: &str,
	content: &str,
	msg: &str,
	date: &str,
) {
	std::fs::write(work.join(file), format!("{content}\n")).unwrap();
	git(w, &["add", "."]);
	git_at(w, date, &["commit", "-q", "-m", msg]);
}

/// `git` with a fixed identity and committer/author date, for deterministic dated commits/merges.
fn git_at(dir: &str, date: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir, "-c", "user.name=T", "-c", "user.email=t@e"];
	full.extend_from_slice(args);
	let out = Command::new("git")
		.args(&full)
		.env("GIT_AUTHOR_DATE", date)
		.env("GIT_COMMITTER_DATE", date)
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn head(w: &str) -> String {
	git(w, &["rev-parse", "HEAD"]).trim().to_owned()
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

fn gta_ok(dir: &str, args: &[&str]) -> bool {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta")
		.status
		.success()
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

/// `git` with a fixed identity, for commit/merge.
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

fn git_ok(dir: &str, args: &[&str]) -> bool {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	Command::new("git")
		.args(&full)
		.output()
		.expect("run git")
		.status
		.success()
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
		let probe = unique_tmp("probe-merge-base");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
