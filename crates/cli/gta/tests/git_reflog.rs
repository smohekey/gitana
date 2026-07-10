//! Reflog parity: `gta` writes `logs/HEAD` and `logs/refs/...` byte-for-byte like stock git for the
//! ref-moving commands that create or move refs — `branch`, `switch` (incl. `-c`), `update-ref`,
//! `symbolic-ref`, and `worktree add`. Each test drives the same command sequence through `gta` and
//! through `git` in parallel repos (with a fixed identity and commit date so the committer line and
//! timestamp are deterministic) and compares the resulting reflog files.
#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

const NAME: &str = "A U Thor";
const EMAIL: &str = "a@example.com";
const DATE: &str = "1700000000 +0000";

/// The author/committer identity and date, fixed so `gta` and `git` produce identical committer
/// lines (and reflog timestamps) — git and gta both read these `GIT_*` variables.
fn envs() -> [(&'static str, &'static str); 6] {
	[
		("GIT_AUTHOR_NAME", NAME),
		("GIT_AUTHOR_EMAIL", EMAIL),
		("GIT_AUTHOR_DATE", DATE),
		("GIT_COMMITTER_NAME", NAME),
		("GIT_COMMITTER_EMAIL", EMAIL),
		("GIT_COMMITTER_DATE", DATE),
	]
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn branch_switch_update_ref_reflogs_match_git_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_local_reflogs("sha256");
}

#[test]
fn branch_switch_update_ref_reflogs_match_git_sha1() {
	check_local_reflogs("sha1");
}

#[test]
fn worktree_add_reflogs_match_git_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_worktree_reflogs("sha256");
}

#[test]
fn worktree_add_reflogs_match_git_sha1() {
	check_worktree_reflogs("sha1");
}

#[test]
fn worktree_add_branch_messages_match_git_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_worktree_branch_messages("sha256");
}

#[test]
fn worktree_add_branch_messages_match_git_sha1() {
	check_worktree_branch_messages("sha1");
}

#[test]
fn detached_head_and_disabled_reflogs_match_git_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_edge_cases("sha256");
}

#[test]
fn detached_head_and_disabled_reflogs_match_git_sha1() {
	check_edge_cases("sha1");
}

#[test]
fn commit_and_reset_reflogs_match_git_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_commit_reset_reflogs("sha256");
}

#[test]
fn commit_and_reset_reflogs_match_git_sha1() {
	check_commit_reset_reflogs("sha1");
}

#[test]
fn disabled_first_commit_writes_no_reflog_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_disabled_first_commit("sha256");
}

#[test]
fn disabled_first_commit_writes_no_reflog_sha1() {
	check_disabled_first_commit("sha1");
}

/// A branch created off a detached HEAD records the literal `HEAD`, and disabling
/// `core.logAllRefUpdates` suppresses new reflogs (branch and the worktree's per-worktree HEAD).
fn check_edge_cases(fmt: &str) {
	let (g, h) = two_repos(fmt);
	let head = gta(&g, &["rev-parse", "HEAD"]).trim().to_owned();

	// Detach HEAD in both (gta cannot detach via checkout yet, so write the file git reads), then a
	// branch created off it records "Created from HEAD", not the object id.
	std::fs::write(g.join(".git/HEAD"), format!("{head}\n")).unwrap();
	std::fs::write(h.join(".git/HEAD"), format!("{head}\n")).unwrap();
	both(&g, &h, &["branch", "off_detached"]);
	assert_log_eq(&g, &h, "logs/refs/heads/off_detached");

	// With reflogs disabled, a new branch is not logged and a worktree add seeds no per-worktree HEAD.
	both(&g, &h, &["config", "core.logAllRefUpdates", "false"]);
	both(&g, &h, &["branch", "unlogged"]);
	assert_absent(&g, "logs/refs/heads/unlogged");
	assert_absent(&h, "logs/refs/heads/unlogged");

	gta(&g, &["worktree", "add", "-b", "wtoff", "../gwtoff"]);
	git(&h, &["worktree", "add", "-b", "wtoff", "../hwtoff"]);
	assert!(
		sole_worktree_head_log_opt(&g).is_none(),
		"gta seeded a per-worktree logs/HEAD despite disabled reflogs"
	);
	assert!(
		sole_worktree_head_log_opt(&h).is_none(),
		"git seeded a per-worktree logs/HEAD despite disabled reflogs (probe assumption wrong)"
	);

	cleanup(&g, &h);
}

/// `commit` and `reset` write `logs/HEAD` and the branch reflog exactly as git does — including
/// git's no-op handling, which the ref-move reflog cascade drives: a `reset` to the current tip
/// records the mirrored `HEAD` entry but not a redundant branch entry (the direct reflog skips a
/// no-op move), and a no-op reset on a detached `HEAD` records nothing at all.
fn check_commit_reset_reflogs(fmt: &str) {
	let (g, h) = two_repos(fmt);

	// A real second commit advances the branch: the branch and HEAD reflogs both gain a `commit:` line.
	std::fs::write(g.join("b.txt"), "x\n").unwrap();
	std::fs::write(h.join("b.txt"), "x\n").unwrap();
	both(&g, &h, &["add", "."]);
	both(&g, &h, &["commit", "-m", "second"]);
	assert_log_eq(&g, &h, "logs/HEAD");
	assert_log_eq(&g, &h, "logs/refs/heads/main");

	// A no-op `reset --hard HEAD`: git records the mirrored HEAD entry (`reset: moving to HEAD`) but
	// leaves the branch reflog untouched. The branch bytes must be unchanged, and both files must
	// still match git.
	let branch_before = std::fs::read(g.join(".git/logs/refs/heads/main")).unwrap();
	both(&g, &h, &["reset", "--hard", "HEAD"]);
	assert_eq!(
		std::fs::read(g.join(".git/logs/refs/heads/main")).unwrap(),
		branch_before,
		"no-op reset must not append a branch reflog entry"
	);
	assert_log_eq(&g, &h, "logs/HEAD");
	assert_log_eq(&g, &h, "logs/refs/heads/main");

	// A real `reset --hard HEAD~1` moves the branch: both reflogs gain a `reset: moving to HEAD~1` line.
	both(&g, &h, &["reset", "--hard", "HEAD~1"]);
	assert_log_eq(&g, &h, "logs/HEAD");
	assert_log_eq(&g, &h, "logs/refs/heads/main");

	// A no-op reset on a *detached* HEAD records nothing (the direct HEAD reflog skips a no-op move,
	// and there is no branch to mirror into). Detach both at the current tip (gta cannot detach via
	// checkout yet, so write the file git also reads), then reset to it.
	let head = gta(&g, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(g.join(".git/HEAD"), format!("{head}\n")).unwrap();
	std::fs::write(h.join(".git/HEAD"), format!("{head}\n")).unwrap();
	let head_log_before = std::fs::read(g.join(".git/logs/HEAD")).unwrap();
	both(&g, &h, &["reset", "--hard", "HEAD"]);
	assert_eq!(
		std::fs::read(g.join(".git/logs/HEAD")).unwrap(),
		head_log_before,
		"no-op detached reset must not append a HEAD reflog entry"
	);
	assert_log_eq(&g, &h, "logs/HEAD");

	cleanup(&g, &h);
}

/// With `core.logAllRefUpdates=false`, the first commit on a branch with no existing reflog writes no
/// reflog at all — matching git, which gates creating a brand-new reflog on the setting (whereas an
/// already-existing reflog is always appended).
fn check_disabled_first_commit(fmt: &str) {
	let base = unique_tmp(&format!("reflog-disabled-{fmt}"));
	let g = base.join("gta");
	let h = base.join("git");
	std::fs::create_dir_all(&g).unwrap();
	std::fs::create_dir_all(&h).unwrap();
	gta(&g, &["init", &format!("--object-format={fmt}")]);
	git(
		&h,
		&[
			"init",
			"-q",
			"-b",
			"main",
			&format!("--object-format={fmt}"),
			".",
		],
	);

	// Disable reflogs before the first commit, so no branch/HEAD reflog exists to fall under git's
	// append-existing carve-out.
	both(&g, &h, &["config", "core.logAllRefUpdates", "false"]);

	std::fs::write(g.join("a.txt"), "hi\n").unwrap();
	std::fs::write(h.join("a.txt"), "hi\n").unwrap();
	both(&g, &h, &["add", "."]);
	both(&g, &h, &["commit", "-m", "initial"]);

	for repo in [&g, &h] {
		assert_absent(repo, "logs/HEAD");
		assert_absent(repo, "logs/refs/heads/main");
	}

	cleanup(&g, &h);
}

/// `branch`, `switch -c`, `switch`, `update-ref`, and `symbolic-ref` write reflogs matching git.
fn check_local_reflogs(fmt: &str) {
	let (g, h) = two_repos(fmt);

	// The initial commit's own reflog already matches (pre-existing behaviour, asserted here as the
	// baseline every later comparison builds on).
	assert_log_eq(&g, &h, "logs/HEAD");
	assert_log_eq(&g, &h, "logs/refs/heads/main");

	// branch with no start point → "Created from <current branch>".
	both(&g, &h, &["branch", "feature"]);
	assert_log_eq(&g, &h, "logs/refs/heads/feature");

	// branch with an explicit start point → "Created from <start as typed>".
	both(&g, &h, &["branch", "topic", "main"]);
	assert_log_eq(&g, &h, "logs/refs/heads/topic");

	// switch -c creates the branch ("Created from HEAD") and appends the checkout line to HEAD.
	both(&g, &h, &["switch", "-c", "newbr"]);
	assert_log_eq(&g, &h, "logs/refs/heads/newbr");
	assert_log_eq(&g, &h, "logs/HEAD");

	// switch to an existing branch appends only the checkout line to HEAD.
	both(&g, &h, &["switch", "feature"]);
	assert_log_eq(&g, &h, "logs/HEAD");

	// A second commit on the current branch (feature) advances it to a distinct id, so the following
	// update-ref is a real move rather than a no-op.
	std::fs::write(g.join("b.txt"), "x\n").unwrap();
	std::fs::write(h.join("b.txt"), "x\n").unwrap();
	both(&g, &h, &["add", "."]);
	both(&g, &h, &["commit", "-m", "second"]);
	let c1 = gta(&g, &["rev-parse", "HEAD~1"]).trim().to_owned();

	// update-ref on the current branch (HEAD → feature) logs the branch and cascades into HEAD, with
	// an empty message (no `-m`).
	both(&g, &h, &["update-ref", "refs/heads/feature", &c1]);
	assert_log_eq(&g, &h, "logs/refs/heads/feature");
	assert_log_eq(&g, &h, "logs/HEAD");

	// A no-op update-ref (the new value equals the current) writes no reflog entry, in either.
	let before = std::fs::read(g.join(".git/logs/refs/heads/feature")).unwrap();
	both(&g, &h, &["update-ref", "refs/heads/feature", &c1]);
	assert_eq!(
		std::fs::read(g.join(".git/logs/refs/heads/feature")).unwrap(),
		before,
		"no-op update-ref should not append a reflog entry"
	);
	assert_log_eq(&g, &h, "logs/refs/heads/feature");

	// update-ref on a ref outside the logged namespaces writes no reflog in either.
	both(&g, &h, &["update-ref", "refs/custom/thing", &c1]);
	assert_absent(&g, "logs/refs/custom/thing");
	assert_absent(&h, "logs/refs/custom/thing");

	// symbolic-ref retargeting HEAD logs an empty-message entry (old resolved value → new target's).
	both(&g, &h, &["symbolic-ref", "HEAD", "refs/heads/topic"]);
	assert_log_eq(&g, &h, "logs/HEAD");

	// Retargeting HEAD at a *symbolic* ref must resolve through the chain (not error and leave HEAD
	// half-changed): point an alias at a branch, then HEAD at the alias.
	both(
		&g,
		&h,
		&["symbolic-ref", "refs/heads/alias", "refs/heads/feature"],
	);
	both(&g, &h, &["symbolic-ref", "HEAD", "refs/heads/alias"]);
	assert_eq!(
		gta(&g, &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/alias",
		"HEAD should be retargeted at the alias"
	);
	assert_log_eq(&g, &h, "logs/HEAD");

	cleanup(&g, &h);
}

/// `worktree add -b` writes the created branch's reflog and the new worktree's per-worktree
/// `logs/HEAD` (a creation line plus a `reset: moving to HEAD` line) matching git.
fn check_worktree_reflogs(fmt: &str) {
	let (g, h) = two_repos(fmt);

	// Distinct checkout paths (the two repos share a parent), so `../gwt` and `../hwt` do not collide.
	gta(&g, &["worktree", "add", "-b", "wtbranch", "../gwt"]);
	git(&h, &["worktree", "add", "-b", "wtbranch", "../hwt"]);

	// The branch the worktree created is logged identically.
	assert_log_eq(&g, &h, "logs/refs/heads/wtbranch");

	// The new worktree's per-worktree logs/HEAD (under the sole admin entry) matches byte-for-byte.
	let gl = sole_worktree_head_log(&g);
	let hl = sole_worktree_head_log(&h);
	assert_eq!(
		String::from_utf8_lossy(&gl),
		String::from_utf8_lossy(&hl),
		"per-worktree logs/HEAD mismatch ({fmt})"
	);

	cleanup(&g, &h);
}

/// `worktree add`'s created-branch reflog message tracks the start point as typed, and says
/// `Reset to` (not `Created from`) when `-B` resets a branch that already exists.
fn check_worktree_branch_messages(fmt: &str) {
	let (g, h) = two_repos(fmt);

	// A second commit so HEAD~1 is a distinct, nameable start point.
	std::fs::write(g.join("b.txt"), "x\n").unwrap();
	std::fs::write(h.join("b.txt"), "x\n").unwrap();
	both(&g, &h, &["add", "."]);
	both(&g, &h, &["commit", "-m", "second"]);

	// `-b <name> <start>` records "Created from <start as typed>".
	gta(&g, &["worktree", "add", "-b", "nb", "../gnb", "HEAD~1"]);
	git(&h, &["worktree", "add", "-b", "nb", "../hnb", "HEAD~1"]);
	assert_log_eq(&g, &h, "logs/refs/heads/nb");

	// `-B <name>` resetting a branch that already exists records "Reset to <start>".
	both(&g, &h, &["branch", "rel", "HEAD~1"]);
	gta(&g, &["worktree", "add", "-B", "rel", "../grel", "HEAD"]);
	git(&h, &["worktree", "add", "-B", "rel", "../hrel", "HEAD"]);
	assert_log_eq(&g, &h, "logs/refs/heads/rel");

	cleanup(&g, &h);
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A gta repo and a git repo, each initialised in `fmt` with an identical initial commit on `main`.
fn two_repos(fmt: &str) -> (PathBuf, PathBuf) {
	let base = unique_tmp(&format!("reflog-{fmt}"));
	let g = base.join("gta");
	let h = base.join("git");
	std::fs::create_dir_all(&g).unwrap();
	std::fs::create_dir_all(&h).unwrap();

	gta(&g, &["init", &format!("--object-format={fmt}")]);
	git(
		&h,
		&[
			"init",
			"-q",
			"-b",
			"main",
			&format!("--object-format={fmt}"),
			".",
		],
	);

	std::fs::write(g.join("a.txt"), "hi\n").unwrap();
	std::fs::write(h.join("a.txt"), "hi\n").unwrap();
	gta(&g, &["add", "."]);
	gta(&g, &["commit", "-m", "initial"]);
	git(&h, &["add", "."]);
	git(&h, &["commit", "-q", "-m", "initial"]);

	// Sanity: identical inputs produce the identical commit id in both.
	assert_eq!(
		gta(&g, &["rev-parse", "HEAD"]).trim(),
		git(&h, &["rev-parse", "HEAD"]).trim(),
		"setup commit ids diverged ({fmt})"
	);
	(g, h)
}

/// Run the same argument vector through `gta` in `g` and `git` in `h`.
fn both(g: &Path, h: &Path, args: &[&str]) {
	gta(g, args);
	git(h, args);
}

/// Assert `logs/<rel>` is byte-identical between the two repos.
fn assert_log_eq(g: &Path, h: &Path, rel: &str) {
	let a = std::fs::read(g.join(".git").join(rel)).unwrap_or_else(|_| panic!("gta {rel} missing"));
	let b = std::fs::read(h.join(".git").join(rel)).unwrap_or_else(|_| panic!("git {rel} missing"));
	assert_eq!(
		String::from_utf8_lossy(&a),
		String::from_utf8_lossy(&b),
		"{rel} mismatch"
	);
}

/// Assert `logs/<rel>` exists in neither, i.e. the ref was not logged.
fn assert_absent(dir: &Path, rel: &str) {
	assert!(
		!dir.join(".git").join(rel).exists(),
		"{rel} should not be logged"
	);
}

/// The bytes of the sole linked worktree's per-worktree `logs/HEAD` under `.git/worktrees/*`.
fn sole_worktree_head_log(dir: &Path) -> Vec<u8> {
	std::fs::read(sole_worktree_admin(dir).join("logs").join("HEAD")).expect("per-worktree logs/HEAD")
}

/// Like [`sole_worktree_head_log`], but `None` when the worktree wrote no per-worktree `logs/HEAD`.
fn sole_worktree_head_log_opt(dir: &Path) -> Option<Vec<u8>> {
	std::fs::read(sole_worktree_admin(dir).join("logs").join("HEAD")).ok()
}

/// The sole linked worktree's admin directory under `.git/worktrees/*`.
fn sole_worktree_admin(dir: &Path) -> PathBuf {
	let worktrees = dir.join(".git").join("worktrees");
	let mut entries = std::fs::read_dir(&worktrees)
		.expect("worktrees dir")
		.filter_map(Result::ok)
		.map(|e| e.path())
		.collect::<Vec<_>>();
	entries.sort();
	entries.into_iter().next().expect("one worktree admin dir")
}

fn cleanup(g: &Path, h: &Path) {
	// Both live under the same unique base directory.
	if let Some(base) = g.parent() {
		let _ = std::fs::remove_dir_all(base);
	}
	let _ = (h,);
}

fn gta(dir: &Path, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir.to_str().unwrap()])
		.args(args)
		.envs(envs())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}

fn git(dir: &Path, args: &[&str]) -> String {
	let mut full = vec!["-C", dir.to_str().unwrap()];
	full.extend_from_slice(args);
	let out = Command::new("git")
		.args(&full)
		.envs(envs())
		.output()
		.expect("run git");
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
	let out = Command::new("git")
		.args(["init", "--object-format=sha256", "--bare"])
		.arg(unique_tmp("sha256-probe"))
		.output();
	matches!(out, Ok(o) if o.status.success())
}
