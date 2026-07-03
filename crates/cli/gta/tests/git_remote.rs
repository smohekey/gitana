//! `gta remote` end-to-end: add/list/set-url/remove the configured remotes, cross-checked against
//! real git — git reads the `[remote "<name>"]` sections gta writes, and `remote remove` drops the
//! remote's tracking refs the way git does.

use std::path::PathBuf;
use std::process::Command;

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	gta(work.to_str().unwrap(), &["init"], b"");
	work
}

#[test]
fn remote_add_list_and_git_interop() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-remote-add");
	let w = work.to_str().unwrap();

	gta(
		w,
		&["remote", "add", "origin", "https://example.com/a.git"],
		b"",
	);
	gta(
		w,
		&["remote", "add", "upstream", "https://example.com/b.git"],
		b"",
	);

	// Bare list is the remote names, sorted.
	assert_eq!(gta(w, &["remote"], b""), "origin\nupstream\n");

	// -v prints fetch/push URLs — byte-for-byte what git prints from the same config.
	let verbose = gta(w, &["remote", "-v"], b"");
	assert_eq!(
		verbose,
		"origin\thttps://example.com/a.git (fetch)\n\
		 origin\thttps://example.com/a.git (push)\n\
		 upstream\thttps://example.com/b.git (fetch)\n\
		 upstream\thttps://example.com/b.git (push)\n"
	);
	assert_eq!(git(w, &["remote", "-v"]), verbose);

	// The config keys are exactly git's: url + the default fetch refspec.
	assert_eq!(
		git(w, &["config", "remote.origin.url"]).trim(),
		"https://example.com/a.git"
	);
	assert_eq!(
		git(w, &["config", "remote.origin.fetch"]).trim(),
		"+refs/heads/*:refs/remotes/origin/*"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_verbose_multi_url_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-multiurl");
	let w = work.to_str().unwrap();

	gta(
		w,
		&["remote", "add", "origin", "https://example.com/a.git"],
		b"",
	);
	// A second URL (git uses the first for fetch and every URL for push) and a pushurl on a second
	// remote — gta's `-v` must reproduce git's exactly for both shapes.
	git(
		w,
		&[
			"remote",
			"set-url",
			"--add",
			"origin",
			"https://example.com/mirror.git",
		],
	);
	gta(
		w,
		&["remote", "add", "backup", "https://example.com/b.git"],
		b"",
	);
	git(
		w,
		&[
			"remote",
			"set-url",
			"--add",
			"--push",
			"backup",
			"https://push.example.com/b.git",
		],
	);
	// A remote with no `url`, only a `pushurl` — git prints `pushonly\t` (no fetch URL) plus a push
	// line. gta must match that too.
	git(
		w,
		&[
			"config",
			"remote.pushonly.pushurl",
			"https://push.example.com/c.git",
		],
	);
	git(
		w,
		&[
			"config",
			"remote.pushonly.fetch",
			"+refs/heads/*:refs/remotes/pushonly/*",
		],
	);
	// A remote whose url/pushurl are the empty string — git treats them as absent in `-v`.
	git(w, &["config", "remote.empty.url", ""]);
	git(w, &["config", "remote.empty.pushurl", ""]);
	git(
		w,
		&[
			"config",
			"remote.empty.fetch",
			"+refs/heads/*:refs/remotes/empty/*",
		],
	);

	assert_eq!(gta(w, &["remote", "-v"], b""), git(w, &["remote", "-v"]));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_verbose_applies_url_rewrites() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-rewrite");
	let w = work.to_str().unwrap();

	// insteadOf rewrites the fetch (and push, when pushInsteadOf does not match); pushInsteadOf
	// rewrites the push URL when it matches the raw URL. gta's `-v` must reproduce git's exactly.
	git(w, &["config", "url.https://github.com/.insteadOf", "gh:"]);
	git(
		w,
		&[
			"config",
			"url.ssh://git@github.com/.pushInsteadOf",
			"https://github.com/",
		],
	);
	// Two rules with equal-length prefixes: git (and gta) keep the first in config order.
	git(
		w,
		&["config", "url.https://first.example/.insteadOf", "dup:"],
	);
	git(
		w,
		&["config", "url.https://second.example/.insteadOf", "dup:"],
	);
	gta(w, &["remote", "add", "a", "gh:owner/repo"], b"");
	gta(w, &["remote", "add", "b", "https://github.com/x/y"], b"");
	// An explicit pushurl that relies on insteadOf rewriting.
	gta(w, &["remote", "add", "c", "https://example.com/c.git"], b"");
	git(w, &["config", "remote.c.pushurl", "gh:owner/c"]);
	// An explicit pushurl that *matches* a pushInsteadOf rule — git applies only insteadOf to it.
	gta(w, &["remote", "add", "d", "https://example.com/d.git"], b"");
	git(
		w,
		&["config", "remote.d.pushurl", "https://github.com/owner/d"],
	);
	// A remote hitting the equal-length tie.
	gta(w, &["remote", "add", "e", "dup:repo"], b"");
	// Interleaved subsections: two tied `insteadOf` prefixes whose file order (not subsection
	// grouping) decides the winner — git and gta must agree.
	git(
		w,
		&[
			"config",
			"--add",
			"url.https://early.example/.insteadOf",
			"tie:",
		],
	);
	git(
		w,
		&[
			"config",
			"--add",
			"url.https://late.example/.pushInsteadOf",
			"z:",
		],
	);
	git(
		w,
		&[
			"config",
			"--add",
			"url.https://late.example/.insteadOf",
			"tie:",
		],
	);
	gta(w, &["remote", "add", "f", "tie:repo"], b"");

	assert_eq!(gta(w, &["remote", "-v"], b""), git(w, &["remote", "-v"]));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_add_accepts_a_quoted_name() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-quote");
	let w = work.to_str().unwrap();

	// A `"` is valid in a remote name; the config writer escapes it in the subsection header, and
	// git must parse the result and list the remote.
	gta(
		w,
		&["remote", "add", "x\"y", "https://example.com/a.git"],
		b"",
	);
	assert!(
		git(w, &["remote"]).lines().any(|l| l == "x\"y"),
		"git lists the quoted remote name"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_set_url_retargets() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-seturl");
	let w = work.to_str().unwrap();

	gta(
		w,
		&["remote", "add", "origin", "https://example.com/old.git"],
		b"",
	);
	gta(
		w,
		&["remote", "set-url", "origin", "https://example.com/new.git"],
		b"",
	);
	assert_eq!(
		git(w, &["config", "remote.origin.url"]).trim(),
		"https://example.com/new.git"
	);
	// The fetch refspec is untouched.
	assert_eq!(
		git(w, &["config", "remote.origin.fetch"]).trim(),
		"+refs/heads/*:refs/remotes/origin/*"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_remove_drops_config_and_tracking_refs() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-remove");
	let w = work.to_str().unwrap();

	gta(
		w,
		&["remote", "add", "origin", "https://example.com/a.git"],
		b"",
	);

	// Tracking refs `remove` must delete: a direct ref, and the usual symbolic `origin/HEAD`.
	git(
		w,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"--allow-empty",
			"-m",
			"seed",
		],
	);
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(w, &["config", "core.logallrefupdates", "true"]); // so update-ref writes a reflog
	git(w, &["update-ref", "refs/remotes/origin/main", &head]);
	git(
		w,
		&[
			"symbolic-ref",
			"refs/remotes/origin/HEAD",
			"refs/remotes/origin/main",
		],
	);
	// Config that names this remote — branch upstream/push and a repo push default — must all go.
	git(w, &["config", "branch.main.remote", "origin"]);
	git(w, &["config", "branch.main.merge", "refs/heads/main"]);
	git(w, &["config", "branch.main.pushRemote", "origin"]);
	git(w, &["config", "remote.pushDefault", "origin"]);
	let direct = work.join(".git/refs/remotes/origin/main");
	let symbolic = work.join(".git/refs/remotes/origin/HEAD");
	let reflog = work.join(".git/logs/refs/remotes/origin/main");
	assert!(
		direct.exists() && symbolic.exists() && reflog.exists(),
		"tracking refs and reflog created"
	);

	gta(w, &["remote", "remove", "origin"], b"");

	// The remote, its tracking refs and reflog, and every config entry naming it are all gone.
	assert_eq!(gta(w, &["remote"], b""), "");
	assert_eq!(git(w, &["remote"]), "");
	assert!(!direct.exists(), "direct tracking ref removed");
	assert!(!symbolic.exists(), "symbolic tracking ref removed");
	assert!(!reflog.exists(), "tracking-ref reflog removed");
	for gone in [
		"branch.main.remote",
		"branch.main.pushRemote",
		"remote.pushDefault",
	] {
		assert!(!git_local_config_ok(w, gone), "config '{gone}' removed");
	}

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_errors_match_git_expectations() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-errors");
	let w = work.to_str().unwrap();

	gta(
		w,
		&["remote", "add", "origin", "https://example.com/a.git"],
		b"",
	);
	// Adding a remote that already exists fails.
	assert!(gta_fail(w, &["remote", "add", "origin", "x"]).contains("already exists"));
	// Operating on a missing remote fails.
	assert!(gta_fail(w, &["remote", "set-url", "nope", "x"]).contains("no such remote"));
	assert!(gta_fail(w, &["remote", "remove", "nope"]).contains("no such remote"));
	// Names that would write an invalid `refs/remotes/<name>/*` refspec (git would then choke on the
	// config) are rejected up front, leaving config unchanged.
	for bad in [
		"a b", "../evil", "a..b", "foo.lock", "@{x}", "x:y", ".hidden", "",
	] {
		gta_fail(w, &["remote", "add", bad, "https://example.com/x.git"]);
	}
	assert_eq!(gta(w, &["remote"], b""), "origin\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn remote_accepts_git_valid_path_names() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-remote-valid");
	let w = work.to_str().unwrap();

	// `<name>` is a middle segment of `refs/remotes/<name>/*`, so git accepts names a whole-refname
	// check would reject, like `@` and a trailing-dot name. gta must accept exactly what git reads.
	for good in ["@", "foo."] {
		gta(
			w,
			&["remote", "add", good, "https://example.com/x.git"],
			b"",
		);
	}
	let mut ours: Vec<String> = gta(w, &["remote"], b"")
		.lines()
		.map(str::to_owned)
		.collect();
	let mut theirs: Vec<String> = git(w, &["remote"]).lines().map(str::to_owned).collect();
	ours.sort();
	theirs.sort();
	assert_eq!(ours, theirs, "git lists the same remotes gta wrote");

	std::fs::remove_dir_all(&work).ok();
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

/// Whether git can read `key` from the *local* config (so an unrelated global cannot mask a removal).
fn git_local_config_ok(dir: &str, key: &str) -> bool {
	Command::new("git")
		.args(["-C", dir, "config", "--local", key])
		.output()
		.expect("run git config")
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
		let probe = unique_tmp("probe-remote");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
