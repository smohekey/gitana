//! `gta status` porcelain layout, cross-checked against `git status --porcelain` with **exact**
//! (unsorted) output — covering git's grouping (tracked changes first, then untracked) and a path
//! that is both a staged change and an untracked working file (e.g. after `rm --cached`).

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn changed_entries_precede_untracked() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-status-order");
	let w = work.to_str().unwrap();

	write(&work, "z.txt", "z\n");
	commit_all(w, "base");
	write(&work, "z.txt", "zz\n"); // modify a tracked path that sorts *after* the untracked one
	write(&work, "a.txt", "a\n"); // untracked

	// git lists the change before the untracked file (not a single global path sort).
	assert_eq!(gta(w, &["status"], b""), git(w, &["status", "--porcelain"]));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_cached_shows_staged_deletion_and_untracked() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-status-rmcached");
	let w = work.to_str().unwrap();

	write(&work, "f.txt", "x\n");
	commit_all(w, "base");
	// `rm --cached` drops the index entry but keeps the working file: a staged deletion *and* a
	// now-untracked file — two porcelain lines for one path.
	gta(w, &["rm", "--cached", "f.txt"], b"");

	let out = gta(w, &["status"], b"");
	assert_eq!(out, git(w, &["status", "--porcelain"]));
	assert!(
		out.contains("D  f.txt") && out.contains("?? f.txt"),
		"{out}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_cached_on_unmerged_path_shows_both_lines() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-status-rmcached-unmerged");
	let w = work.to_str().unwrap();

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

	gta(w, &["rm", "--cached", "f.txt"], b"");
	assert_eq!(gta(w, &["status"], b""), git(w, &["status", "--porcelain"]));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn honors_ignorecase_info_exclude_and_excludes_file() {
	// End-to-end: `gta status` must consult git's three standard exclude sources for untracked
	// detection — a case-folded `.gitignore` (`core.ignoreCase`), `.git/info/exclude`, and the global
	// `core.excludesFile` — exactly as `git status` does. Previously it read only `.gitignore`,
	// case-sensitively, so it over-reported untracked files here.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-status-excludes");
	let w = work.to_str().unwrap();
	write(&work, "a.txt", "a\n");
	commit_all(w, "base");

	// (1) core.ignoreCase: an UPPER-case `.gitignore` rule folds onto a lower-case file.
	write(&work, ".gitignore", "*.LOG\n");
	git(w, &["config", "core.ignoreCase", "true"]);
	write(&work, "debug.log", "log\n");
	// (2) `.git/info/exclude`.
	std::fs::write(work.join(".git/info/exclude"), b"*.tmp\n").unwrap();
	write(&work, "scratch.tmp", "t\n");
	// (3) `core.excludesFile`, kept inside `.git` so neither scan lists the excludes file itself.
	let excludes = work.join(".git/custom_excludes");
	std::fs::write(&excludes, b"*.bak\n").unwrap();
	git(
		w,
		&["config", "core.excludesFile", excludes.to_str().unwrap()],
	);
	write(&work, "old.bak", "b\n");
	// A plainly untracked file none of the sources cover.
	write(&work, "keep.txt", "k\n");

	let theirs = git(w, &["status", "--porcelain"]);
	assert!(
		!theirs.contains("debug.log")
			&& !theirs.contains("scratch.tmp")
			&& !theirs.contains("old.bak")
			&& theirs.contains("keep.txt"),
		"sanity: git omits the three excluded files and lists keep.txt: {theirs}"
	);
	assert_eq!(
		gta(w, &["status"], b""),
		theirs,
		"gta status must honour ignoreCase + info/exclude + excludesFile like git"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn honors_worktree_config_ignorecase_override() {
	// End-to-end (native effective-config path): `gta status` must honour a per-worktree `core.ignoreCase`
	// override in `config.worktree` (with `extensions.worktreeConfig`), exactly as `git status` does —
	// the override beats the common value (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-status-wtcfg-ic");
	let w = work.to_str().unwrap();
	write(&work, ".gitignore", "*.LOG\n");
	write(&work, "a.txt", "a\n");
	commit_all(w, "base");
	git(w, &["config", "extensions.worktreeConfig", "true"]);
	git(w, &["config", "core.ignoreCase", "true"]); // common: fold
	git(w, &["config", "--worktree", "core.ignoreCase", "false"]); // override: no fold
	write(&work, "debug.log", "log\n");

	let theirs = git(w, &["status", "--porcelain"]);
	assert!(
		theirs.contains("debug.log"),
		"sanity: git honours the false override (debug.log untracked): {theirs}"
	);
	assert_eq!(
		gta(w, &["status"], b""),
		theirs,
		"gta status must honour the per-worktree core.ignoreCase override like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rejects_valueless_excludes_file() {
	// A valueless `core.excludesFile` (`[core]\n\texcludesFile`) is fatal to git ("missing value"); gta
	// must reject it too — on `status`, and on `add -f`, which skips reading the excludes file but still
	// validates the setting (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-valueless-excludes");
	let w = work.to_str().unwrap();
	write(&work, "a.txt", "a\n");
	commit_all(w, "base");
	let cfg = work.join(".git/config");
	let content = format!(
		"{}[core]\n\texcludesFile\n",
		std::fs::read_to_string(&cfg).unwrap()
	);
	std::fs::write(&cfg, content).unwrap();
	write(&work, "u.txt", "u\n");

	// Sanity: git aborts on the valueless setting.
	assert!(
		!Command::new("git")
			.args(["-C", w, "status", "--porcelain"])
			.status()
			.expect("run git")
			.success(),
		"sanity: git rejects a valueless core.excludesFile"
	);
	for args in [vec!["status"], vec!["add", "-f", "u.txt"]] {
		let out = assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(["-C", w])
			.args(&args)
			.output()
			.expect("run gta");
		assert!(
			!out.status.success(),
			"gta {args:?} must reject a valueless core.excludesFile"
		);
	}
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn honors_bare_tilde_excludes_file_with_empty_home() {
	// `core.excludesFile=~` under an EMPTY `HOME`: git treats the bare-tilde expansion as no excludes file and
	// continues (probed vs git 2.55: exit 0, the untracked file is listed), rather than resolving `~` to the
	// worktree root and aborting on a directory. `gta status` must match — the shared excludes resolver may
	// not fail in sanitized/container environments where `HOME=`.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-status-baretilde");
	let w = work.to_str().unwrap();
	write(&work, "a.txt", "a\n");
	commit_all(w, "base");
	git(w, &["config", "core.excludesFile", "~"]);
	write(&work, "u.txt", "u\n");

	let theirs = Command::new("git")
		.args(["-C", w, "status", "--porcelain"])
		.env("HOME", "")
		.output()
		.expect("run git");
	assert!(
		theirs.status.success(),
		"sanity: git tolerates a bare `~` excludesFile with empty HOME"
	);
	let theirs = String::from_utf8(theirs.stdout).unwrap();
	assert!(
		theirs.contains("u.txt"),
		"sanity: git lists the untracked file: {theirs}"
	);

	let ours = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "status"])
		.env("HOME", "")
		.output()
		.expect("run gta");
	assert!(
		ours.status.success(),
		"gta status must tolerate a bare `~` excludesFile with empty HOME like git: {}",
		String::from_utf8_lossy(&ours.stderr)
	);
	assert_eq!(String::from_utf8(ours.stdout).unwrap(), theirs);
	std::fs::remove_dir_all(&work).ok();
}

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	work
}

fn write(work: &Path, name: &str, content: &str) {
	std::fs::write(work.join(name), content).unwrap();
}

fn commit_all(w: &str, msg: &str) -> String {
	git(w, &["add", "."]);
	git(w, &["commit", "-q", "-m", msg]);
	git(w, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn checkout_new(w: &str, branch: &str, start: &str) {
	git(w, &["checkout", "-q", "-b", branch, start]);
}

fn merge_expecting_conflict(w: &str, branch: &str) {
	let ok = Command::new("git")
		.args(["-C", w, "merge", branch])
		.output()
		.expect("run git merge")
		.status
		.success();
	assert!(!ok, "git merge {branch} was expected to conflict");
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
		let probe = unique_tmp("probe-status");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
