//! Differential harness: run `gta` and real `git --object-format=sha256` on the
//! same fixtures and assert they agree (repo bytes + plumbing output).

use std::path::PathBuf;
use std::process::Command;

#[test]
fn plumbing_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-plumbing");
	let w = work.to_str().unwrap();

	// gta init produces a repo git recognises.
	gta(w, &["init"], b"");
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
	assert_eq!(
		git(w, &["config", "extensions.objectformat"]).trim(),
		"sha256"
	);

	// hash-object: gta -w == git hash-object, and git can read what gta wrote.
	let content = b"hello gitana\n";
	let gta_oid = gta(w, &["hash-object", "-w", "--stdin"], content);
	let git_oid = git_stdin(w, &["hash-object", "--stdin"], content);
	assert_eq!(
		gta_oid.trim(),
		git_oid.trim(),
		"hash-object oid must match git"
	);
	assert_eq!(
		git(w, &["cat-file", "-p", gta_oid.trim()]).as_bytes(),
		content
	);

	// cat-file type/size/content match git.
	let oid = gta_oid.trim();
	assert_eq!(
		gta(w, &["cat-file", "-t", oid], b""),
		git(w, &["cat-file", "-t", oid])
	);
	assert_eq!(
		gta(w, &["cat-file", "-s", oid], b""),
		git(w, &["cat-file", "-s", oid])
	);
	assert_eq!(
		gta(w, &["cat-file", "-p", oid], b""),
		git(w, &["cat-file", "-p", oid])
	);

	// ls-tree (and -r) match git on a git-built tree.
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/b.txt"), b"b\n").unwrap();
	git(w, &["add", "."]);
	let tree = git(w, &["write-tree"]);
	let tree = tree.trim();
	assert_eq!(gta(w, &["ls-tree", tree], b""), git(w, &["ls-tree", tree]));
	assert_eq!(
		gta(w, &["ls-tree", "-r", tree], b""),
		git(w, &["ls-tree", "-r", tree])
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn refs_and_revisions_match_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-refs");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(work.join("a.txt"), b"2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");
	let c2 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// rev-parse: refs, HEAD, ancestry.
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), c2);
	assert_eq!(gta(w, &["rev-parse", "main"], b"").trim(), c2);
	assert_eq!(gta(w, &["rev-parse", "HEAD~1"], b"").trim(), c1);

	// rev-list and ls-files match git.
	assert_eq!(
		gta(w, &["rev-list", "HEAD"], b""),
		git(w, &["rev-list", "HEAD"])
	);
	assert_eq!(gta(w, &["ls-files"], b""), git(w, &["ls-files"]));

	// symbolic-ref reads HEAD.
	assert_eq!(
		gta(w, &["symbolic-ref", "HEAD"], b"").trim(),
		"refs/heads/main"
	);

	// update-ref creates a branch git then resolves.
	gta(w, &["update-ref", "refs/heads/feature", &c1], b"");
	assert_eq!(git(w, &["rev-parse", "feature"]).trim(), c1);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn porcelain_cycle_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-porcelain");
	let w = work.to_str().unwrap();
	let env: [(&str, &str); 6] = [
		("GIT_AUTHOR_NAME", "A"),
		("GIT_AUTHOR_EMAIL", "a@x"),
		("GIT_AUTHOR_DATE", "1700000000 +0000"),
		("GIT_COMMITTER_NAME", "C"),
		("GIT_COMMITTER_EMAIL", "c@x"),
		("GIT_COMMITTER_DATE", "1700000000 +0000"),
	];

	gta(w, &["init"], b"");
	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/b.txt"), b"world\n").unwrap();

	// add . then status matches git after staging.
	gta(w, &["add", "."], b"");
	assert_eq!(
		sorted(&gta(w, &["status"], b"")),
		sorted(&git(w, &["status", "--porcelain=v1"]))
	);

	// gta commit produces a commit byte-identical to git's (same inputs).
	gta_env(w, &["commit", "-m", "first"], &env);
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	let tree = git(w, &["write-tree"]).trim().to_owned();
	let git_commit = git_env(w, &["commit-tree", &tree, "-m", "first"], &env)
		.trim()
		.to_owned();
	assert_eq!(
		head, git_commit,
		"gta commit must equal git's commit object"
	);
	git(w, &["fsck", "--no-dangling"]);

	// log shows the commit, and the tree is clean afterwards.
	assert!(gta(w, &["log"], b"").contains(&head));
	assert!(gta(w, &["status"], b"").is_empty(), "clean after commit");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn branches_tags_switch_match_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-branch");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// gta creates a branch and a lightweight tag git then resolves.
	gta(w, &["branch", "feature"], b"");
	gta(w, &["tag", "v1"], b"");
	assert_eq!(git(w, &["rev-parse", "feature"]).trim(), c1);
	assert_eq!(git(w, &["rev-parse", "v1"]).trim(), c1);

	// Listing matches git's (current marked with `* `, names sorted).
	assert_eq!(gta(w, &["branch"], b""), git(w, &["branch"]));
	assert_eq!(gta(w, &["tag"], b""), git(w, &["tag"]));

	// switch moves HEAD; a commit on the new branch then vanishes on switch back.
	gta(w, &["switch", "feature"], b"");
	assert_eq!(
		git(w, &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/feature"
	);
	std::fs::write(work.join("b.txt"), b"2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");
	gta(w, &["switch", "main"], b"");
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
	assert!(
		!work.join("b.txt").exists(),
		"b.txt removed on switch to main"
	);

	// switch -c creates and checks out in one step.
	gta(w, &["switch", "-c", "topic"], b"");
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/topic");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn diff_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-diff");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("f.txt"), b"a\nb\nc\nd\ne\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "base");

	// Unstaged change: gta diff agrees with git diff on the changed payload.
	std::fs::write(work.join("f.txt"), b"a\nB\nc\nd\nE\n").unwrap();
	std::fs::write(work.join("new.txt"), b"fresh\n").unwrap();
	git(w, &["add", "new.txt"]); // stage the add so it shows in --cached, not diff
	assert_eq!(
		diff_payload(&gta(w, &["diff"], b"")),
		diff_payload(&git(w, &["diff"]))
	);

	// Staged change: gta diff --cached agrees with git diff --cached.
	git(w, &["add", "f.txt"]);
	assert_eq!(
		diff_payload(&gta(w, &["diff", "--cached"], b"")),
		diff_payload(&git(w, &["diff", "--cached"]))
	);

	std::fs::remove_dir_all(&work).ok();
}

/// The semantic content of a unified diff: every added/removed line (sign + text),
/// sorted, ignoring file headers, hunk headers, and the no-newline marker. Used to
/// compare gta's diff to git's without depending on git's exact byte framing.
fn diff_payload(text: &str) -> Vec<String> {
	let mut out: Vec<String> = text
		.lines()
		.filter(|l| {
			(l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---")
		})
		.map(str::to_owned)
		.collect();
	out.sort();
	out
}

fn gta_env(dir: &str, args: &[&str], env: &[(&str, &str)]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.envs(env.iter().copied())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}

fn git_env(dir: &str, args: &[&str], env: &[(&str, &str)]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git")
		.args(&full)
		.envs(env.iter().copied())
		.output()
		.expect("run git");
	assert!(out.status.success(), "git {args:?} failed");
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn sorted(text: &str) -> Vec<String> {
	let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
	lines.sort();
	lines
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

fn git_stdin(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	use std::io::Write;
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let mut child = Command::new("git")
		.args(&full)
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.spawn()
		.expect("spawn git");
	child.stdin.take().unwrap().write_all(stdin).unwrap();
	let out = child.wait_with_output().unwrap();
	assert!(out.status.success());
	String::from_utf8(out.stdout).unwrap()
}

fn unique_tmp(tag: &str) -> PathBuf {
	// A per-call sequence number keeps every temp dir distinct even for the same tag, so
	// tests running in parallel threads never share a path (the `git` subprocesses they
	// spawn would otherwise race on it).
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
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
		let probe = unique_tmp("gta-probe");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
