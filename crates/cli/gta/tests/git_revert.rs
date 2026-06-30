//! `gta revert` end-to-end: recording a commit that undoes a previous one's change — authored by the
//! current user (not the original author) — and the conflict lifecycle (`--abort` / `--continue` /
//! `gta commit`). Cross-checked against real git where deterministic: the reverted tree matches stock
//! `git revert`, and git agrees the conflicted index is `UU`.

use std::path::{Path, PathBuf};
use std::process::Command;

const ORIG_AUTHOR: &str = "Original Author <orig@example.com>";

#[test]
fn clean_revert_authored_by_reverter_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-revert-clean");
	let w = work.to_str().unwrap();

	write(&work, "f.txt", "a\n");
	commit_all(w, "A");
	write(&work, "f.txt", "a\nb\n");
	let target = commit_as(w, "add b", ORIG_AUTHOR); // the commit to revert (HEAD)

	gta(w, &["revert", &target], b"");

	// A single-parent commit on the old tip, with the change undone.
	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), target);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // not a merge — no second parent
	assert_eq!(std::fs::read_to_string(work.join("f.txt")).unwrap(), "a\n");

	// Authored by the reverter (the config identity), NOT the original author.
	let head = git(w, &["cat-file", "-p", "HEAD"]);
	assert!(
		head.contains("author T <t@e>"),
		"reverter is the author: {head}"
	);
	assert!(!head.contains(ORIG_AUTHOR), "not the original author");
	// git's revert message.
	assert!(head.contains("Revert \"add b\""), "{head}");
	assert!(
		head.contains(&format!("This reverts commit {target}.")),
		"{head}"
	);
	assert!(gta(w, &["status"], b"").is_empty(), "clean after revert");
	let gta_tree = gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim().to_owned();

	// Oracle: stock git reverts the same commit; the resulting trees must match.
	git(w, &["branch", "gitcheck", &target]);
	git(w, &["checkout", "-q", "gitcheck"]);
	git(w, &["revert", "--no-edit", &target]);
	assert_eq!(gta_tree, git(w, &["rev-parse", "HEAD^{tree}"]).trim());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn conflicting_revert_materialises_state() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-conflict");
	let w = work.to_str().unwrap();
	let (_, target) = setup_conflict(&work, w);

	let (stdout, _) = gta_out_fail(w, &["revert", &target]);
	assert!(stdout.contains("CONFLICT"), "conflict reported: {stdout}");

	assert_eq!(
		std::fs::read_to_string(work.join(".git/REVERT_HEAD"))
			.unwrap()
			.trim(),
		target
	);
	let f = std::fs::read_to_string(work.join("f.txt")).unwrap();
	assert!(
		f.contains("<<<<<<<") && f.contains(">>>>>>>"),
		"markers: {f}"
	);
	assert!(gta(w, &["status"], b"").contains("UU f.txt"));
	assert!(
		git(w, &["status", "--porcelain"]).contains("UU f.txt"),
		"git agrees the index is UU"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn revert_abort_restores_state() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-abort");
	let w = work.to_str().unwrap();
	let (head, target) = setup_conflict(&work, w);

	gta_out_fail(w, &["revert", &target]);
	gta(w, &["revert", "--abort"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), head); // unchanged
	assert_eq!(std::fs::read_to_string(work.join("f.txt")).unwrap(), "C\n");
	assert!(!work.join(".git/REVERT_HEAD").exists());
	assert!(gta(w, &["status"], b"").is_empty(), "clean after abort");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn revert_continue_makes_a_single_parent_commit() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-continue");
	let w = work.to_str().unwrap();
	let (head, target) = setup_conflict(&work, w);

	gta_out_fail(w, &["revert", &target]);
	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");
	gta(w, &["revert", "--continue"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), head);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // single parent
	assert!(git(w, &["cat-file", "-p", "HEAD"]).contains("author T <t@e>"));
	assert!(!work.join(".git/REVERT_HEAD").exists());
	assert!(gta(w, &["status"], b"").is_empty());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn commit_concludes_a_revert() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-commit");
	let w = work.to_str().unwrap();
	let (head, target) = setup_conflict(&work, w);

	gta_out_fail(w, &["revert", &target]);
	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");
	gta(w, &["commit", "-m", "resolved revert"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), head);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // single parent
	assert!(!work.join(".git/REVERT_HEAD").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn empty_revert_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-empty");
	let w = work.to_str().unwrap();

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	write(&work, "b.txt", "b\n");
	let target = commit_all(w, "add b"); // adds b.txt

	gta(w, &["revert", &target], b""); // first revert removes b.txt
	let tip = gta(w, &["rev-parse", "HEAD"], b"").trim().to_owned();
	// Reverting it again is a no-op: git refuses an empty revert.
	gta_fail(w, &["revert", &target]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), tip); // unchanged

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn revert_refused_with_staged_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-revert-staged");
	let w = work.to_str().unwrap();

	write(&work, "f.txt", "a\n");
	commit_all(w, "A");
	write(&work, "f.txt", "a\nb\n");
	let target = commit_all(w, "add b");
	// Stage an unrelated change; git refuses a revert with a dirty index.
	write(&work, "staged.txt", "s\n");
	gta(w, &["add", "staged.txt"], b"");

	gta_fail(w, &["revert", &target]);
	assert!(work.join("staged.txt").exists(), "staged work preserved");
	assert!(!work.join(".git/REVERT_HEAD").exists());

	std::fs::remove_dir_all(&work).ok();
}

/// Build a revert conflict on f.txt: base, then `target` sets it to B, then a later commit sets it to
/// C. Reverting `target` (B→base) conflicts with the C change. Returns `(head_tip, target)`.
fn setup_conflict(work: &Path, w: &str) -> (String, String) {
	write(work, "f.txt", "base\n");
	commit_all(w, "base");
	write(work, "f.txt", "B\n");
	let target = commit_all(w, "to B");
	write(work, "f.txt", "C\n");
	let head = commit_all(w, "to C");
	(head, target)
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

fn commit_as(w: &str, msg: &str, author: &str) -> String {
	git(w, &["add", "."]);
	git(
		w,
		&["commit", "-q", &format!("--author={author}"), "-m", msg],
	);
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
		let probe = unique_tmp("probe-revert");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
