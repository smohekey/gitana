//! `gta merge` end-to-end: fast-forward, true two-parent merge commits, `--no-ff` / `--ff-only`,
//! already-up-to-date, and clean refusal of a conflicting merge — cross-checked against real git
//! where the result is deterministic (the merged tree matches `git merge-tree --write-tree`).

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn fast_forward() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-merge-ff");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "b.txt", "b\n");
	let b = commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);

	gta(w, &["merge", "feature"], b"");
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), b); // fast-forwarded to feature
	assert!(work.join("b.txt").exists());
	assert!(
		gta(w, &["status"], b"").is_empty(),
		"clean after fast-forward"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn clean_true_merge_has_two_parents_and_gits_tree() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-true");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "f\n");
	let feature = commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let main_tip = commit_all(w, "M");

	gta(w, &["merge", "feature"], b"");

	// A two-parent merge commit: first parent the old branch tip, second the merged commit.
	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), main_tip);
	assert_eq!(gta(w, &["rev-parse", "HEAD^2"], b"").trim(), feature);
	// The merged tree is exactly the one git's three-way merge produces.
	let git_tree = git(w, &["merge-tree", "--write-tree", &main_tip, &feature]);
	assert_eq!(
		gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim(),
		git_tree.lines().next().unwrap().trim()
	);
	assert!(work.join("f.txt").exists() && work.join("m.txt").exists());
	assert!(gta(w, &["status"], b"").is_empty(), "clean after merge");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn no_ff_forces_a_merge_commit() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-noff");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "a.txt", "a\n");
	let a = commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "b.txt", "b\n");
	let b = commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);

	gta(w, &["merge", "--no-ff", "feature"], b"");
	// Not a fast-forward: a new merge commit with both tips as parents and feature's tree.
	assert_ne!(gta(w, &["rev-parse", "HEAD"], b"").trim(), b);
	assert_eq!(gta(w, &["rev-parse", "HEAD^1"], b"").trim(), a);
	assert_eq!(gta(w, &["rev-parse", "HEAD^2"], b"").trim(), b);
	assert_eq!(
		gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim(),
		git(w, &["rev-parse", &format!("{b}^{{tree}}")]).trim()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn ff_only_refuses_a_divergent_merge() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-ffonly");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "base.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "f\n");
	commit_all(w, "F");
	git(w, &["checkout", "-q", &main]);
	write(&work, "m.txt", "m\n");
	let main_tip = commit_all(w, "M");

	gta_fail(w, &["merge", "--ff-only", "feature"]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip); // unchanged

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn already_up_to_date() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-uptodate");
	let w = work.to_str().unwrap();

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	git(w, &["branch", "base"]); // points at A, an ancestor of HEAD
	write(&work, "b.txt", "b\n");
	let tip = commit_all(w, "B");

	let out = gta(w, &["merge", "base"], b"");
	assert!(out.contains("Already up to date"), "{out}");
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), tip); // unchanged

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn conflicting_merge_is_refused_without_touching_anything() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-conflict");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "f.txt", "base\n");
	commit_all(w, "base");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "THEIRS\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", &main]);
	write(&work, "f.txt", "OURS\n");
	let main_tip = commit_all(w, "ours");

	gta_fail(w, &["merge", "feature"]);
	// HEAD, the working tree, and the merge state are all untouched.
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip);
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"OURS\n"
	);
	assert!(!work.join(".git/MERGE_HEAD").exists());
	assert!(gta(w, &["status"], b"").is_empty(), "still clean");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn criss_cross_merges_cleanly_like_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-crisscross");
	let w = work.to_str().unwrap();

	// Two commits a, b off a root, then two independent merges of both (m1, m2) — a criss-cross
	// whose merge has two bases ({a, b}). The merge must reduce them and resolve cleanly, as git does.
	write(&work, "base.txt", "base\n");
	let root = commit_all(w, "root");
	git(w, &["checkout", "-q", "-b", "abr"]);
	write(&work, "x.txt", "x\n");
	let a = commit_all(w, "a");
	git(w, &["checkout", "-q", "-b", "bbr", &root]);
	write(&work, "y.txt", "y\n");
	commit_all(w, "b");
	git(w, &["checkout", "-q", "-b", "m1br", &a]);
	git(w, &["merge", "-q", "--no-edit", "bbr"]);
	let m1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(w, &["checkout", "-q", "-b", "m2br", &a]);
	git(w, &["merge", "-q", "--no-edit", "bbr"]);
	let m2 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(w, &["checkout", "-q", "m1br"]);

	gta(w, &["merge", "m2br"], b"");
	let git_tree = git(w, &["merge-tree", "--write-tree", &m1, &m2]);
	assert_eq!(
		gta(w, &["rev-parse", "HEAD^{tree}"], b"").trim(),
		git_tree.lines().next().unwrap().trim()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn already_up_to_date_with_dirty_worktree() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-uptodate-dirty");
	let w = work.to_str().unwrap();

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	git(w, &["branch", "base"]); // an ancestor of HEAD
	write(&work, "a.txt", "a2\n");
	let tip = commit_all(w, "B");
	write(&work, "a.txt", "dirty\n"); // uncommitted local edit

	// git reports "Already up to date." and leaves the dirty file alone — no cleanliness error.
	let out = gta(w, &["merge", "base"], b"");
	assert!(out.contains("Already up to date"), "{out}");
	assert_eq!(
		std::fs::read_to_string(work.join("a.txt")).unwrap(),
		"dirty\n"
	);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), tip);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn detached_head_fast_forward() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-detached");
	let w = work.to_str().unwrap();

	write(&work, "a.txt", "a\n");
	let a = commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "b.txt", "b\n");
	let b = commit_all(w, "B");
	git(w, &["checkout", "-q", &a]); // detach HEAD at A

	gta(w, &["merge", "feature"], b"");
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), b); // detached HEAD fast-forwarded
	assert!(work.join("b.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_preserves_an_unrelated_dirty_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-dirty-unrelated");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "a.txt", "a\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "b.txt", "b\n"); // feature only adds b.txt; a.txt is untouched
	let b = commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);
	write(&work, "a.txt", "dirty\n"); // unrelated local edit

	// The merge only touches b.txt, so it proceeds and leaves the dirty a.txt alone, like git.
	gta(w, &["merge", "feature"], b"");
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), b);
	assert_eq!(
		std::fs::read_to_string(work.join("a.txt")).unwrap(),
		"dirty\n"
	);
	assert!(work.join("b.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn merge_refuses_when_a_touched_path_is_dirty() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-dirty-touched");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "f.txt", "base\n");
	let a = commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "feature\n"); // feature changes f.txt
	commit_all(w, "B");
	git(w, &["checkout", "-q", &main]);
	write(&work, "f.txt", "dirty\n"); // dirty on the same path the merge would overwrite

	gta_fail(w, &["merge", "feature"]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), a); // unchanged
	assert_eq!(
		std::fs::read_to_string(work.join("f.txt")).unwrap(),
		"dirty\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn refuses_to_merge_with_an_unconcluded_merge() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-merge-inprogress");
	let w = work.to_str().unwrap();
	let main = head_branch(w);

	write(&work, "f.txt", "base\n");
	commit_all(w, "A");
	git(w, &["checkout", "-q", "-b", "other"]);
	write(&work, "o.txt", "o\n"); // a divergent branch to try merging later
	commit_all(w, "O");
	git(w, &["checkout", "-q", &main]);
	git(w, &["checkout", "-q", "-b", "feature"]);
	write(&work, "f.txt", "THEIRS\n");
	commit_all(w, "theirs");
	git(w, &["checkout", "-q", &main]);
	write(&work, "f.txt", "OURS\n");
	let main_tip = commit_all(w, "ours");

	// Start a conflicting merge with git (writes MERGE_HEAD), then resolve and stage it — the index
	// is clean again, but the merge is not concluded.
	let conflicted = Command::new("git")
		.args(["-C", w, "merge", "feature"])
		.output()
		.unwrap();
	assert!(!conflicted.status.success(), "git merge should conflict");
	write(&work, "f.txt", "resolved\n");
	git(w, &["add", "f.txt"]);
	assert!(work.join(".git/MERGE_HEAD").exists());

	// gta must refuse another merge while MERGE_HEAD remains, as git does.
	gta_fail(w, &["merge", "other"]);
	assert_eq!(gta(w, &["rev-parse", "HEAD"], b"").trim(), main_tip); // no new commit
	assert!(work.join(".git/MERGE_HEAD").exists()); // state preserved

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
		let probe = unique_tmp("probe-merge");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
