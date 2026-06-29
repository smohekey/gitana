//! `gta config` end-to-end: read/write the local `.git/config`, cross-checked against real git
//! (git reads what gta writes and vice versa).

use std::path::PathBuf;
use std::process::Command;

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	gta(work.to_str().unwrap(), &["init"], b"");
	work
}

#[test]
fn config_set_get_and_git_interop() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-config-set");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	assert_eq!(gta(w, &["config", "user.name"], b""), "A U Thor\n");
	assert_eq!(gta(w, &["config", "--get", "user.name"], b""), "A U Thor\n");
	// git reads what gta wrote.
	assert_eq!(git(w, &["config", "user.name"]).trim(), "A U Thor");

	// ...and gta reads what git wrote.
	git(w, &["config", "user.email", "a@example.com"]);
	assert_eq!(gta(w, &["config", "user.email"], b""), "a@example.com\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_subsection_round_trips() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-sub");
	let w = work.to_str().unwrap();

	gta(w, &["config", "remote.origin.url", "http://example/x"], b"");
	assert_eq!(
		gta(w, &["config", "remote.origin.url"], b""),
		"http://example/x\n"
	);
	assert_eq!(
		git(w, &["config", "remote.origin.url"]).trim(),
		"http://example/x"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_add_and_get_all() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-multi");
	let w = work.to_str().unwrap();

	gta(w, &["config", "--add", "remote.origin.fetch", "one"], b"");
	gta(w, &["config", "--add", "remote.origin.fetch", "two"], b"");
	assert_eq!(
		gta(w, &["config", "--get-all", "remote.origin.fetch"], b""),
		"one\ntwo\n"
	);
	// git sees both values too.
	assert_eq!(
		git(w, &["config", "--get-all", "remote.origin.fetch"]),
		"one\ntwo\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_unset_removes_the_key() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-unset");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	gta(w, &["config", "--unset", "user.name"], b"");
	// The key is gone from the local config: reading exits non-zero, and git agrees it is unset
	// locally (a `--local` lookup, so an unrelated global `user.name` cannot mask the removal).
	gta_fail(w, &["config", "user.name"]);
	assert!(
		!Command::new("git")
			.args(["-C", w, "config", "--local", "user.name"])
			.output()
			.unwrap()
			.status
			.success()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_bool_and_int() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-typed");
	let w = work.to_str().unwrap();

	gta(w, &["config", "core.flag", "yes"], b"");
	assert_eq!(gta(w, &["config", "--bool", "core.flag"], b""), "true\n");
	gta(w, &["config", "pack.size", "2k"], b"");
	assert_eq!(gta(w, &["config", "--int", "pack.size"], b""), "2048\n");
	assert_eq!(git(w, &["config", "--int", "pack.size"]).trim(), "2048");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_list_includes_set_values() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-list");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	let list = gta(w, &["config", "--list"], b"");
	assert!(
		list.lines().any(|l| l == "user.name=A U Thor"),
		"list: {list}"
	);
	assert!(
		list.lines().any(|l| l == "extensions.objectformat=sha256"),
		"list: {list}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_unset_rejects_a_value_pattern() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-unset-value");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "Alice"], b"");
	// `--unset key value` is a value-pattern op in git; we reject it rather than deleting blindly,
	// so the key is preserved.
	gta_fail(w, &["config", "--unset", "user.name", "Bob"]);
	assert_eq!(gta(w, &["config", "user.name"], b""), "Alice\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_bare_variable_is_present_with_empty_value() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-bare");
	let w = work.to_str().unwrap();

	// Append a valueless variable directly (git itself always writes `= value`).
	let cfg = work.join(".git/config");
	let mut text = std::fs::read_to_string(&cfg).unwrap();
	text.push_str("[core]\n\tbareflag\n");
	std::fs::write(&cfg, text).unwrap();

	// Present but valueless: an empty line, exit 0 — matching git, and distinct from a missing key.
	assert_eq!(gta(w, &["config", "core.bareflag"], b""), "\n");
	assert_eq!(gta(w, &["config", "--get-all", "core.bareflag"], b""), "\n");
	assert_eq!(git(w, &["config", "core.bareflag"]), "\n");
	// A bare variable reads as boolean-true.
	assert_eq!(
		gta(w, &["config", "--bool", "core.bareflag"], b""),
		"true\n"
	);
	// ...but reading it as an integer is a parse error (present, not absent), like git.
	let err = gta_fail(w, &["config", "--int", "core.bareflag"]);
	assert!(err.contains("not an integer"), "stderr: {err}");
	assert!(
		!Command::new("git")
			.args(["-C", w, "config", "--int", "core.bareflag"])
			.output()
			.unwrap()
			.status
			.success(),
		"git also rejects --int of a bare variable"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_missing_key_exits_nonzero() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-missing");
	let w = work.to_str().unwrap();

	gta_fail(w, &["config", "no.such.key"]);

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
		let probe = unique_tmp("probe-config");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
