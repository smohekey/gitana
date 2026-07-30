#![cfg(unix)]

//! `gta add`'s ignored-path advisory, checked byte-for-byte against stock `git` as the oracle: the
//! stderr message, the non-zero exit, `-f`/`--force` overriding it, and `advice.addIgnoredFile=false`
//! suppressing the two `hint:` lines. SHA-1 is used for both repos so a stock `git` without SHA-256
//! support can serve as the oracle.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `gta -C dir <args>`, returning the raw output (no success assertion — the advisory exits 1).
fn gta_raw(dir: &Path, args: &[&str]) -> Output {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir.to_str().unwrap()])
		.args(args)
		.output()
		.expect("run gta")
}

/// Run `git -C dir <args>`, returning the raw output.
fn git_raw(dir: &Path, args: &[&str]) -> Output {
	let mut full = vec!["-C", dir.to_str().unwrap()];
	full.extend_from_slice(args);
	Command::new("git").args(&full).output().expect("run git")
}

/// Assert a `git` invocation succeeded.
fn git_ok(dir: &Path, args: &[&str]) {
	let out = git_raw(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
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

/// Initialise `dir` via `init` (`git_init` or `gta_init`), then lay out an identical SHA-1 repo: a
/// `.gitignore` of `ign/` + `*.log`, and the untracked files `ign/new`, `root.log`, `keep`.
fn setup(dir: &Path, init: impl Fn(&Path)) {
	init(dir);
	std::fs::write(dir.join(".gitignore"), b"ign/\n*.log\n").unwrap();
	std::fs::create_dir_all(dir.join("ign")).unwrap();
	std::fs::write(dir.join("ign/new"), b"n\n").unwrap();
	std::fs::write(dir.join("root.log"), b"l\n").unwrap();
	std::fs::write(dir.join("keep"), b"k\n").unwrap();
}

/// `gta add` on ignored paths must match `git add` byte-for-byte on stderr and exit status, across a
/// single ignored file, a multi-path add that still stages the non-ignored path, and a named ignored
/// directory.
#[test]
fn add_ignored_advisory_matches_git() {
	let cases: &[&[&str]] = &[
		&["add", "ign/new"],
		&["add", "ign/new", "root.log", "keep"],
		&["add", "ign"],
	];
	for case in cases {
		let g = unique_tmp("add-adv-git");
		let t = unique_tmp("add-adv-gta");
		setup(&g, git_init);
		setup(&t, gta_init);

		let git_out = git_raw(&g, case);
		let gta_out = gta_raw(&t, case);

		assert_eq!(
			String::from_utf8_lossy(&gta_out.stderr),
			String::from_utf8_lossy(&git_out.stderr),
			"stderr mismatch for {case:?}"
		);
		assert_eq!(
			gta_out.status.code(),
			git_out.status.code(),
			"exit code mismatch for {case:?}"
		);
		// The staged result matches too (git reads the gta-written SHA-1 index directly).
		assert_eq!(
			git(&t, &["diff", "--cached", "--name-only"]),
			git(&g, &["diff", "--cached", "--name-only"]),
			"staged set mismatch for {case:?}"
		);

		std::fs::remove_dir_all(&g).ok();
		std::fs::remove_dir_all(&t).ok();
	}
}

/// `advice.addIgnoredFile=false` suppresses the two `hint:` lines (keeping the header, the path list,
/// and the non-zero exit) — matching git.
#[test]
fn add_ignored_advisory_respects_advice_config() {
	let g = unique_tmp("add-adv-cfg-git");
	let t = unique_tmp("add-adv-cfg-gta");
	setup(&g, git_init);
	setup(&t, gta_init);
	git_ok(&g, &["config", "advice.addIgnoredFile", "false"]);
	git_ok(&t, &["config", "advice.addIgnoredFile", "false"]);

	let git_out = git_raw(&g, &["add", "ign/new"]);
	let gta_out = gta_raw(&t, &["add", "ign/new"]);
	assert_eq!(
		String::from_utf8_lossy(&gta_out.stderr),
		String::from_utf8_lossy(&git_out.stderr),
		"suppressed-advisory stderr mismatch"
	);
	assert!(
		!String::from_utf8_lossy(&gta_out.stderr).contains("hint:"),
		"hint lines must be suppressed"
	);
	assert_eq!(gta_out.status.code(), git_out.status.code());

	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&t).ok();
}

/// `gta add -f` (and `--force`) stages an otherwise-ignored file and exits 0, matching `git add -f`.
#[test]
fn add_force_stages_ignored_file() {
	for flag in ["-f", "--force"] {
		let t = unique_tmp("add-force-gta");
		setup(&t, gta_init);
		let out = gta_raw(&t, &["add", flag, "ign/new"]);
		assert!(
			out.status.success(),
			"gta add {flag} ign/new failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		assert!(
			git(&t, &["diff", "--cached", "--name-only"])
				.lines()
				.any(|l| l == "ign/new"),
			"the ignored file is staged with {flag}"
		);
		std::fs::remove_dir_all(&t).ok();
	}
}

/// `gta add -f` bypasses ignore during traversal too — a broad `.`, a named directory, and a glob all
/// stage the ignored content they cover, matching `git add -f` byte-for-byte on the staged set and exit.
#[test]
fn add_force_walk_forms_match_git() {
	let layout = |d: &Path| {
		std::fs::write(d.join(".gitignore"), b"*.log\nign/\n").unwrap();
		std::fs::write(d.join("a.log"), b"a\n").unwrap();
		std::fs::create_dir_all(d.join("ign")).unwrap();
		std::fs::write(d.join("ign/new"), b"n\n").unwrap();
		std::fs::create_dir_all(d.join("sub")).unwrap();
		std::fs::write(d.join("sub/x.log"), b"x\n").unwrap();
		std::fs::write(d.join("sub/keep"), b"k\n").unwrap();
	};
	let cases: &[&[&str]] = &[
		&["add", "-f", "."],
		&["add", "-f", "sub"],
		&["add", "-f", "*.log"],
	];
	for case in cases {
		let g = unique_tmp("add-fw-git");
		let t = unique_tmp("add-fw-gta");
		git_init(&g);
		layout(&g);
		gta_init(&t);
		layout(&t);
		let git_out = git_raw(&g, case);
		let gta_out = gta_raw(&t, case);
		assert_eq!(
			gta_out.status.code(),
			git_out.status.code(),
			"exit mismatch for {case:?}"
		);
		assert_eq!(
			git(&t, &["diff", "--cached", "--name-only"]),
			git(&g, &["diff", "--cached", "--name-only"]),
			"staged set mismatch for {case:?}"
		);
		std::fs::remove_dir_all(&g).ok();
		std::fs::remove_dir_all(&t).ok();
	}
}

/// The advisory lists reported paths in git's lexicographic (byte) order, not argument order — a
/// multi-path add given out of order matches git's sorted advisory.
#[test]
fn add_ignored_advisory_is_sorted_like_git() {
	let layout = |d: &Path| {
		std::fs::write(d.join(".gitignore"), b"*.log\nzdir/\nadir/\n").unwrap();
		std::fs::create_dir_all(d.join("zdir")).unwrap();
		std::fs::create_dir_all(d.join("adir")).unwrap();
		std::fs::write(d.join("zdir/x"), b"x\n").unwrap();
		std::fs::write(d.join("adir/y"), b"y\n").unwrap();
		std::fs::write(d.join("a.log"), b"a\n").unwrap();
		std::fs::write(d.join("m.log"), b"m\n").unwrap();
	};
	let g = unique_tmp("add-sort-git");
	let t = unique_tmp("add-sort-gta");
	git_init(&g);
	layout(&g);
	gta_init(&t);
	layout(&t);
	let args = &["add", "zdir/x", "a.log", "adir/y", "m.log"];
	assert_eq!(
		String::from_utf8_lossy(&gta_raw(&t, args).stderr),
		String::from_utf8_lossy(&git_raw(&g, args).stderr),
		"advisory ordering must be git's lexicographic order"
	);
	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&t).ok();
}

/// A malformed `advice.addIgnoredFile` fails the add before staging anything — git reads and validates
/// the setting during `add` setup, before touching the index.
#[test]
fn add_rejects_malformed_advice_config_before_staging() {
	let t = unique_tmp("add-badcfg-gta");
	gta_init(&t);
	std::fs::write(t.join(".gitignore"), b"ign/\n").unwrap();
	std::fs::create_dir_all(t.join("ign")).unwrap();
	std::fs::write(t.join("ign/new"), b"n\n").unwrap();
	std::fs::write(t.join("keep"), b"k\n").unwrap();
	git_ok(&t, &["config", "advice.addIgnoredFile", "notabool"]);

	let out = gta_raw(&t, &["add", "ign/new", "keep"]);
	assert!(
		!out.status.success(),
		"a malformed advice.addIgnoredFile must fail the add"
	);
	// Restore a valid value so the index can be inspected — stock `git` also refuses to read the tree
	// with the malformed boolean present (the point of the config check).
	git_ok(&t, &["config", "advice.addIgnoredFile", "true"]);
	assert_eq!(
		git(&t, &["diff", "--cached", "--name-only"]),
		"",
		"nothing is staged when the config is rejected up front"
	);
	std::fs::remove_dir_all(&t).ok();
}

/// `gta init --object-format=sha1` in `dir` (SHA-1 so stock `git` can act as the oracle).
fn gta_init(dir: &Path) {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["init", "--object-format=sha1", dir.to_str().unwrap()])
		.output()
		.expect("gta init");
}

/// `git init -q -b main` in `dir`.
fn git_init(dir: &Path) {
	git_ok(dir, &["init", "-q", "-b", "main", dir.to_str().unwrap()]);
}

/// Run `git`/`gta` (per `gta`) in `dir` with a fixed identity in the environment (for `commit`),
/// returning the raw output.
fn tool_raw(dir: &Path, gta: bool, args: &[&str]) -> Output {
	let ident = [
		("GIT_AUTHOR_NAME", "T"),
		("GIT_AUTHOR_EMAIL", "t@e"),
		("GIT_COMMITTER_NAME", "T"),
		("GIT_COMMITTER_EMAIL", "t@e"),
	];
	if gta {
		let mut c = assert_cmd::Command::cargo_bin("gta").unwrap();
		c.args(["-C", dir.to_str().unwrap()]).args(args);
		for (k, v) in ident {
			c.env(k, v);
		}
		c.output().expect("run gta")
	} else {
		let mut full = vec!["-C", dir.to_str().unwrap()];
		full.extend_from_slice(args);
		let mut c = Command::new("git");
		c.args(&full);
		for (k, v) in ident {
			c.env(k, v);
		}
		c.output().expect("run git")
	}
}

/// Run a tool step, asserting success.
fn tool_ok(dir: &Path, gta: bool, args: &[&str]) {
	let out = tool_raw(dir, gta, args);
	assert!(
		out.status.success(),
		"{} {args:?} failed: {}",
		if gta { "gta" } else { "git" },
		String::from_utf8_lossy(&out.stderr)
	);
}

/// Build an identical SHA-1 sparse repo: tracked `out/a`, `out/b`, `ign/tracked` (committed), a
/// `.gitignore` of `ign/`, then a cone sparse-checkout that includes `ign/` and excludes `out/`
/// (removing `out/*` from disk); finally an untracked, ignored `ign/new`.
fn sparse_setup(dir: &Path, gta: bool) {
	if gta {
		gta_init(dir);
	} else {
		git_init(dir);
	}
	std::fs::create_dir_all(dir.join("out")).unwrap();
	std::fs::create_dir_all(dir.join("ign")).unwrap();
	std::fs::write(dir.join("out/a"), b"a\n").unwrap();
	std::fs::write(dir.join("out/b"), b"b\n").unwrap();
	std::fs::write(dir.join("ign/tracked"), b"t\n").unwrap();
	std::fs::write(dir.join(".gitignore"), b"ign/\n").unwrap();
	tool_ok(
		dir,
		gta,
		&["add", "-f", "out/a", "out/b", "ign/tracked", ".gitignore"],
	);
	tool_ok(dir, gta, &["commit", "-m", "base"]);
	// Cone mode is git's `--cone` flag but gta's default (`--no-cone` opts out), so init differs per tool.
	if gta {
		tool_ok(dir, gta, &["sparse-checkout", "init"]);
	} else {
		tool_ok(dir, gta, &["sparse-checkout", "init", "--cone"]);
	}
	tool_ok(dir, gta, &["sparse-checkout", "set", "ign"]);
	std::fs::write(dir.join("ign/new"), b"n\n").unwrap();
}

/// `gta add`'s sparse-checkout advisory matches git's block byte-for-byte: the multi-line header, the
/// out-of-cone pathspecs in argument order, the four `hint:` lines, and the non-zero exit — with
/// `advice.updateSparsePath=false` suppressing the hints.
#[test]
fn add_sparse_advisory_matches_git() {
	// A repeated out-of-cone pathspec is listed once per occurrence (git preserves duplicates).
	for case in [
		&["add", "out/b", "out/a"][..],
		&["add", "out/a"][..],
		&["add", "out/a", "out/a"][..],
	] {
		let g = unique_tmp("add-sp-git");
		let t = unique_tmp("add-sp-gta");
		sparse_setup(&g, false);
		sparse_setup(&t, true);
		let git_out = git_raw(&g, case);
		let gta_out = gta_raw(&t, case);
		assert_eq!(
			String::from_utf8_lossy(&gta_out.stderr),
			String::from_utf8_lossy(&git_out.stderr),
			"sparse advisory mismatch for {case:?}"
		);
		assert_eq!(
			gta_out.status.code(),
			git_out.status.code(),
			"exit for {case:?}"
		);
		std::fs::remove_dir_all(&g).ok();
		std::fs::remove_dir_all(&t).ok();
	}

	// advice.updateSparsePath=false suppresses the hint lines.
	let g = unique_tmp("add-sp-cfg-git");
	let t = unique_tmp("add-sp-cfg-gta");
	sparse_setup(&g, false);
	sparse_setup(&t, true);
	git_ok(&g, &["config", "advice.updateSparsePath", "false"]);
	git_ok(&t, &["config", "advice.updateSparsePath", "false"]);
	let git_out = git_raw(&g, &["add", "out/a"]);
	let gta_out = gta_raw(&t, &["add", "out/a"]);
	assert_eq!(
		String::from_utf8_lossy(&gta_out.stderr),
		String::from_utf8_lossy(&git_out.stderr),
		"suppressed sparse advisory mismatch"
	);
	assert!(
		!String::from_utf8_lossy(&gta_out.stderr).contains("hint:"),
		"sparse hint lines must be suppressed"
	);
	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&t).ok();
}

/// When a single `add` hits both an out-of-cone pathspec and an ignored pathspec, git prints both
/// blocks — sparse first, then ignored. `gta` matches byte-for-byte.
#[test]
fn add_sparse_and_ignored_advisories_together_match_git() {
	let g = unique_tmp("add-both-git");
	let t = unique_tmp("add-both-gta");
	sparse_setup(&g, false);
	sparse_setup(&t, true);
	let case = &["add", "ign/new", "out/a"];
	let git_out = git_raw(&g, case);
	let gta_out = gta_raw(&t, case);
	assert_eq!(
		String::from_utf8_lossy(&gta_out.stderr),
		String::from_utf8_lossy(&git_out.stderr),
		"combined sparse+ignored advisory mismatch"
	);
	assert_eq!(gta_out.status.code(), git_out.status.code());
	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&t).ok();
}

/// The sparse advisory reports a *discovered* untracked out-of-cone path once even across repeated
/// sweeps (`add . .`), and prefers that concrete path over the glob text when a glob matches both it and
/// a tracked skip-worktree entry (`add out/*` → `out/new`, not `out/*`). `gta` matches git byte-for-byte.
#[test]
fn add_sparse_advisory_dedups_discovered_and_prefers_concrete() {
	for case in [&["add", ".", "."][..], &["add", "out/*"][..]] {
		let g = unique_tmp("add-spd-git");
		let t = unique_tmp("add-spd-gta");
		// sparse_setup leaves `out/` out-of-cone with tracked `out/a`/`out/b`; add an untracked out/new.
		sparse_setup(&g, false);
		sparse_setup(&t, true);
		std::fs::create_dir_all(g.join("out")).unwrap();
		std::fs::create_dir_all(t.join("out")).unwrap();
		std::fs::write(g.join("out/new"), b"n\n").unwrap();
		std::fs::write(t.join("out/new"), b"n\n").unwrap();

		let git_out = git_raw(&g, case);
		let gta_out = gta_raw(&t, case);
		assert_eq!(
			String::from_utf8_lossy(&gta_out.stderr),
			String::from_utf8_lossy(&git_out.stderr),
			"sparse advisory mismatch for {case:?}"
		);
		std::fs::remove_dir_all(&g).ok();
		std::fs::remove_dir_all(&t).ok();
	}
}

/// Run `git -C dir <args>`, asserting success and returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_raw(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
