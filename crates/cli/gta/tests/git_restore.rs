//! `gta restore` end-to-end: working-tree, staged, and combined path restoration without moving
//! `HEAD`, with the resulting state cross-checked against real git.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn restore_worktree_discards_unstaged_edits() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-restore-worktree");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// With no target flag, `restore` rewrites the working tree from the index.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	gta(w, &["restore", "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A\n");
	assert!(git(w, &["status", "--porcelain"]).is_empty());
	// HEAD never moves during path restore.
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn restore_staged_unstages_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-restore-staged");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// Stage an edit to a tracked file and a brand-new file.
	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	std::fs::write(work.join("new.txt"), b"NEW\n").unwrap();
	git(w, &["add", "a.txt", "new.txt"]);

	// `restore --staged` resets both index entries from HEAD: the tracked edit becomes unstaged,
	// the new file becomes untracked. Working-tree files are untouched.
	gta(w, &["restore", "--staged", "a.txt", "new.txt"], b"");
	assert!(git(w, &["diff", "--cached", "--name-only"]).is_empty());
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");
	assert_eq!(std::fs::read(work.join("new.txt")).unwrap(), b"NEW\n");

	let mut status: Vec<String> = git(w, &["status", "--porcelain"])
		.lines()
		.map(str::to_owned)
		.collect();
	status.sort();
	assert_eq!(status, vec![" M a.txt".to_owned(), "?? new.txt".to_owned()]);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn restore_staged_and_worktree_from_source() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-restore-sw");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");

	// `-S -W --source=<c1>` restores both the index and the working tree from the first commit.
	gta(
		w,
		&[
			"restore",
			"--staged",
			"--worktree",
			"--source",
			&c1,
			"a.txt",
		],
		b"",
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert!(
		git(w, &["diff", "--name-only"]).is_empty(),
		"the working tree matches the index"
	);
	assert_eq!(
		git(w, &["diff", "--cached", "--name-only"]).trim(),
		"a.txt",
		"the index differs from HEAD (staged)"
	);
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn restore_worktree_from_source_leaves_index() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-restore-wt-source");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");

	// A worktree-only restore from a tree-ish rewrites the file but does not stage it.
	gta(w, &["restore", "--source", &c1, "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert!(
		git(w, &["diff", "--cached", "--name-only"]).is_empty(),
		"the index is untouched"
	);
	assert_eq!(
		git(w, &["diff", "--name-only"]).trim(),
		"a.txt",
		"the working-tree change is unstaged"
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
		let probe = unique_tmp("probe-restore");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
