//! `gta` operating inside a **linked worktree** (`git worktree add`), where `.git` is a file and
//! git splits the repository between a per-worktree directory (`HEAD`, `index`) and a shared common
//! directory (`objects`, `refs`, `config`). gta must read `HEAD` from the worktree but objects and
//! branch refs from the common dir, and a commit must advance only that worktree's branch.
//!
//! Cross-checked against stock `git` so the routing matches git's own behaviour.

use std::path::PathBuf;
use std::process::Command;

/// gta reads `HEAD`, the branch tip, objects, and config through a linked worktree, and a commit
/// made there advances only that worktree's branch — leaving the main worktree's branch untouched.
#[test]
fn reads_and_commits_through_a_linked_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_linked_worktree("sha256");
}

/// The original report: `gta log` in a SHA-1 linked worktree (stock git's default format) failed
/// with "invalid ref content" before the common/per-worktree split. Lock that in for SHA-1 too.
#[test]
fn reads_and_commits_through_a_sha1_linked_worktree() {
	check_linked_worktree("sha1");
}

fn check_linked_worktree(object_format: &str) {
	let base = unique_tmp(&format!("gta-worktree-{object_format}"));
	let main = base.join("main");
	let wt = base.join("wt");
	let main_s = main.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	// A main repo with a `base` commit on `main`, and a `feature` branch checked out into a linked
	// worktree alongside it.
	std::fs::create_dir_all(&main).unwrap();
	git(
		main_s,
		&[
			"init",
			"-q",
			&format!("--object-format={object_format}"),
			".",
		],
	);
	git(main_s, &["config", "user.name", "T"]);
	git(main_s, &["config", "user.email", "t@e"]);
	std::fs::write(main.join("f.txt"), "base\n").unwrap();
	git(main_s, &["add", "."]);
	git(main_s, &["commit", "-q", "-m", "base"]);
	let base_commit = git(main_s, &["rev-parse", "HEAD"]).trim().to_owned();
	git(main_s, &["branch", "feature"]);
	git(main_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// `.git` in the linked worktree is a file, not a directory — the case gta used to choke on.
	assert!(
		wt.join(".git").is_file(),
		"linked worktree .git should be a file"
	);

	// HEAD is read from the per-worktree dir; the branch tip and objects from the common dir.
	assert_eq!(
		gta(wt_s, &["rev-parse", "HEAD"], b"").trim(),
		git(wt_s, &["rev-parse", "HEAD"]).trim(),
	);
	assert_eq!(gta(wt_s, &["rev-parse", "HEAD"], b"").trim(), base_commit);
	// A read that walks the graph (objects live in the common dir) names the base commit.
	assert!(gta(wt_s, &["log"], b"").contains(&base_commit));
	// A clean worktree matches git's porcelain (empty).
	assert_eq!(
		gta(wt_s, &["status"], b""),
		git(wt_s, &["status", "--porcelain"])
	);

	// A commit made in the linked worktree: writes a blob to the common object store and advances the
	// worktree's branch (`feature`), not the main worktree's (`main`).
	std::fs::write(wt.join("g.txt"), "from-worktree\n").unwrap();
	gta(wt_s, &["add", "g.txt"], b"");
	let new_commit = gta(wt_s, &["commit", "-m", "add g"], b"").trim().to_owned();

	// `feature` moved to the new commit; `main` did not move.
	assert_eq!(git(main_s, &["rev-parse", "feature"]).trim(), new_commit);
	assert_eq!(git(main_s, &["rev-parse", "main"]).trim(), base_commit);
	// The new commit and its blob are stored where stock git can read them (the common object store).
	assert_eq!(
		git(wt_s, &["cat-file", "-p", "HEAD:g.txt"]),
		"from-worktree\n"
	);
	assert_eq!(git(wt_s, &["rev-parse", "HEAD"]).trim(), new_commit);

	std::fs::remove_dir_all(&base).ok();
}

/// A branch's ref is shared across worktrees, so gta must refuse to check out a branch already
/// checked out in another worktree — as git does — rather than putting two worktrees on one branch.
#[test]
fn switch_refuses_a_branch_checked_out_in_another_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let base = unique_tmp("gta-worktree-switch");
	let main = base.join("main");
	let wt = base.join("wt");
	let main_s = main.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	std::fs::create_dir_all(&main).unwrap();
	git(main_s, &["init", "-q", "--object-format=sha256", "."]);
	git(main_s, &["config", "user.name", "T"]);
	git(main_s, &["config", "user.email", "t@e"]);
	std::fs::write(main.join("f.txt"), "base\n").unwrap();
	git(main_s, &["add", "."]);
	git(main_s, &["commit", "-q", "-m", "base"]);
	git(main_s, &["branch", "feature"]);
	git(main_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// The linked worktree cannot switch to `main` (held by the main worktree), and the main worktree
	// cannot switch to `feature` (held by the linked worktree) — both refused, HEADs unmoved.
	assert!(gta_fail(wt_s, &["switch", "main"]).contains("already checked out"));
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"feature"
	);

	assert!(gta_fail(main_s, &["switch", "feature"]).contains("already checked out"));
	assert_eq!(
		git(main_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"main"
	);

	// A branch no other worktree holds is fine: creating and switching to a fresh branch works.
	gta(wt_s, &["switch", "-c", "fresh"], b"");
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"fresh"
	);

	std::fs::remove_dir_all(&base).ok();
}

/// A *bare* common repository has no main working tree, so its symbolic `HEAD` is not a checkout —
/// a linked worktree may switch to that branch. The bare flag must be read with git's boolean
/// grammar (`core.bare = yes`, not just `true`), or gta would wrongly treat the bare HEAD as a
/// second checkout and refuse.
#[test]
fn switch_allows_the_branch_named_by_a_bare_repo_head() {
	let base = unique_tmp("gta-worktree-bare");
	let seed = base.join("seed");
	let bare = base.join("bare.git");
	let wt = base.join("wt");
	let base_s = base.to_str().unwrap();
	let seed_s = seed.to_str().unwrap();
	let bare_s = bare.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	// A bare repo (default sha1) whose HEAD names `main`, with a `feature` branch parked in a linked
	// worktree. `core.bare` is written in git's `yes` form, not the literal `true`.
	std::fs::create_dir_all(&seed).unwrap();
	git(seed_s, &["init", "-q", "-b", "main", "."]);
	git(seed_s, &["config", "user.name", "T"]);
	git(seed_s, &["config", "user.email", "t@e"]);
	std::fs::write(seed.join("f.txt"), "base\n").unwrap();
	git(seed_s, &["add", "."]);
	git(seed_s, &["commit", "-q", "-m", "base"]);
	git(base_s, &["clone", "-q", "--bare", seed_s, "bare.git"]);
	git(bare_s, &["config", "core.bare", "yes"]);
	git(bare_s, &["branch", "feature", "main"]);
	git(bare_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// git allows it (the bare HEAD is not a checkout); gta must too.
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"feature"
	);
	gta(wt_s, &["switch", "main"], b"");
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"main"
	);

	std::fs::remove_dir_all(&base).ok();
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

/// Run `gta` expecting a non-zero exit; return its stderr.
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
		let probe = unique_tmp("probe-worktree");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
