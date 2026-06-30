//! `gta cherry-pick` end-to-end: re-applying a commit's change as a single-parent commit that
//! preserves the original author, and the conflict lifecycle (`--abort` / `--continue` / `gta
//! commit`). Cross-checked against real git where deterministic — the picked tree matches stock
//! `git cherry-pick`, and git agrees the conflicted index is `UU`.

use std::path::{Path, PathBuf};
use std::process::Command;

const PICK_AUTHOR: &str = "Cherry Picker <cp@example.com>";

#[test]
fn clean_pick_preserves_author_and_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-cp-clean");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "c.txt", "c\n");
	let feature = commit_as(w, "F", PICK_AUTHOR); // the commit to pick, authored distinctly
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let main_tip = commit_all(w, "M");

	gta(w, &["cherry-pick", &feature], b"");

	// A single-parent commit on the old tip, with the change applied and the original author kept.
	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), main_tip);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // not a merge — no second parent
	assert!(work.join("c.txt").exists() && work.join("m.txt").exists());
	assert!(
		git(w, &["cat-file", "-p", "HEAD"]).contains(PICK_AUTHOR),
		"author preserved"
	);
	assert!(gta(w, &["status"], b"").is_empty(), "clean after pick");
	let gta_tree = gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim().to_owned();

	// Oracle: stock git cherry-picks the same commit onto the same base; the trees must match.
	git(w, &["branch", "gitcheck", &main_tip]);
	git(w, &["checkout", "-q", "gitcheck"]);
	git(w, &["cherry-pick", &feature]);
	assert_eq!(gta_tree, git(w, &["rev-parse", "HEAD^{tree}"]).trim());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn conflicting_pick_materialises_state() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-conflict");
	let w = work.to_str().unwrap();
	let (_, feature) = setup_conflict(&work, w);

	let (stdout, _) = gta_out_fail(w, &["cherry-pick", &feature]);
	assert!(stdout.contains("CONFLICT"), "conflict reported: {stdout}");

	assert_eq!(
		std::fs::read_to_string(work.join(".git/CHERRY_PICK_HEAD"))
			.unwrap()
			.trim(),
		feature
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
fn cherry_pick_abort_restores_pre_pick_state() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-abort");
	let w = work.to_str().unwrap();
	let (main_tip, feature) = setup_conflict(&work, w);

	gta_out_fail(w, &["cherry-pick", &feature]);
	gta(w, &["cherry-pick", "--abort"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip); // unchanged
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"OURS\n"
	);
	assert!(!work.join(".git/CHERRY_PICK_HEAD").exists());
	assert!(gta(w, &["status"], b"").is_empty(), "clean after abort");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn cherry_pick_continue_makes_a_single_parent_commit() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-continue");
	let w = work.to_str().unwrap();
	let (main_tip, feature) = setup_conflict(&work, w);

	gta_out_fail(w, &["cherry-pick", &feature]);
	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");
	gta(w, &["cherry-pick", "--continue"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), main_tip);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // single parent
	assert!(
		git(w, &["cat-file", "-p", "HEAD"]).contains(PICK_AUTHOR),
		"author preserved through --continue"
	);
	assert!(!work.join(".git/CHERRY_PICK_HEAD").exists());
	assert!(gta(w, &["status"], b"").is_empty());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn commit_concludes_a_cherry_pick() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-commit");
	let w = work.to_str().unwrap();
	let (main_tip, feature) = setup_conflict(&work, w);

	gta_out_fail(w, &["cherry-pick", &feature]);
	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");
	gta(w, &["commit", "-m", "resolved pick"], b"");

	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), main_tip);
	gta_fail(w, &["rev-parse", "HEAD^2"]); // single parent
	assert!(git(w, &["cat-file", "-p", "HEAD"]).contains(PICK_AUTHOR));
	assert!(!work.join(".git/CHERRY_PICK_HEAD").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn empty_pick_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-empty");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "c.txt", "c\n");
	let feature = commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	// Apply the same change on main, so re-picking feature is a no-op.
	write(&work, "c.txt", "c\n");
	let tip = commit_all(w, "same change");

	gta_fail(w, &["cherry-pick", &feature]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), tip); // unchanged

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn pick_refused_with_staged_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-staged");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "pick.txt", "pick\n");
	let feature = commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	// Stage an unrelated change that is not yet committed.
	write(&work, "staged.txt", "staged\n");
	git(w, &["add", "staged.txt"]);

	// git refuses a cherry-pick with a dirty index; gta must too — without eating the staged file.
	let git_cp = Command::new("git")
		.args(["-C", w, "cherry-pick", &feature])
		.output()
		.unwrap();
	assert!(!git_cp.status.success(), "git refuses a dirty-index pick");
	git(w, &["cherry-pick", "--quit"]); // clear any partial git sequencer state

	gta_fail(w, &["cherry-pick", &feature]);
	assert!(work.join("staged.txt").exists(), "staged work preserved");
	assert!(git(w, &["status", "--porcelain"]).contains("A  staged.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn pick_on_detached_head_refused_without_mutating() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-detached");
	let w = work.to_str().unwrap();

	write(&work, "base.txt", "base\n");
	let base = commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "pick.txt", "pick\n");
	let feature = commit_all(w, "F");
	git(w, &["checkout", "-q", &base]); // detach HEAD at base

	gta_fail(w, &["cherry-pick", &feature]);
	// Nothing applied: pick.txt is neither in the work tree nor staged, HEAD unmoved.
	assert!(!work.join("pick.txt").exists());
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), base);
	assert!(gta(w, &["status"], b"").is_empty(), "index untouched");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_refused_while_a_cherry_pick_is_in_progress() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-merge-block");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	// A divergent branch to try merging mid-cherry-pick.
	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "other"]);
	write(&work, "o.txt", "o\n");
	commit_all(w, "O");
	git(w, &["checkout", "-q", &main]);
	let (main_tip, feature) = append_conflict(&work, w);

	// Conflict, then resolve the index (but do not conclude the cherry-pick).
	gta_out_fail(w, &["cherry-pick", &feature]);
	write(&work, "f.txt", "resolved\n");
	gta(w, &["add", "f.txt"], b"");

	gta_fail(w, &["merge", "other"]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip); // unmoved
	assert!(work.join(".git/CHERRY_PICK_HEAD").exists(), "state intact");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn empty_completion_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-cp-empty-continue");
	let w = work.to_str().unwrap();
	let (main_tip, feature) = setup_conflict(&work, w);

	gta_out_fail(w, &["cherry-pick", &feature]);
	// Resolve the conflict back to HEAD's content: nothing to commit.
	write(&work, "f.txt", "OURS\n");
	gta(w, &["add", "f.txt"], b"");

	gta_fail(w, &["cherry-pick", "--continue"]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip); // no commit
	assert!(
		work.join(".git/CHERRY_PICK_HEAD").exists(),
		"state preserved"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Like [`setup_conflict`] but assumes a `base.txt` commit already exists (the caller built other
/// branches first); adds the conflicting f.txt on a fresh `feature` and on the current branch.
fn append_conflict(work: &Path, w: &str) -> (String, String) {
	let main = head_branch(w);
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(work, "f.txt", "THEIRS\n");
	let feature = commit_all(w, "theirs");
	git(w, &["checkout", "-q", &main]);
	write(work, "f.txt", "OURS\n");
	let main_tip = commit_all(w, "ours");
	(main_tip, feature)
}

/// Build a content conflict for cherry-pick: base, `feature` edits f.txt (THEIRS, distinct author),
/// main edits f.txt (OURS). Leaves the main branch checked out; returns `(main_tip, feature_tip)`.
fn setup_conflict(work: &Path, w: &str) -> (String, String) {
	let main = head_branch(w);
	write(work, "f.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(work, "f.txt", "THEIRS\n");
	let feature = commit_as(w, "theirs", PICK_AUTHOR);
	git(w, &["checkout", "-q", &main]);
	write(work, "f.txt", "OURS\n");
	let main_tip = commit_all(w, "ours");
	(main_tip, feature)
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
		let probe = unique_tmp("probe-cp");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
