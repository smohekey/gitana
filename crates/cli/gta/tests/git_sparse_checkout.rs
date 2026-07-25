//! `gta sparse-checkout` end-to-end: the input validations git enforces on `set`/`add`, cross-checked
//! against stock git's behaviour (probed against git 2.50.1). A cone directory argument must be a
//! directory (not a tracked file) and carry no leading slash; a non-cone `set`/`add` must run from the
//! work-tree root.

use std::path::PathBuf;
use std::process::Command;

/// A repo (`gta init`) with committed `a/f`, `a/sub/g`, `b/h`, `root.txt`. Returns the work dir.
fn repo(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	std::fs::create_dir_all(work.join("a/sub")).unwrap();
	std::fs::create_dir_all(work.join("b")).unwrap();
	std::fs::write(work.join("a/f"), b"1\n").unwrap();
	std::fs::write(work.join("a/sub/g"), b"2\n").unwrap();
	std::fs::write(work.join("b/h"), b"3\n").unwrap();
	std::fs::write(work.join("root.txt"), b"r\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "base");
	work
}

/// A cone `set` narrows the working tree to the named directory; git reads gitana's skip-worktree bits.
#[test]
fn cone_set_narrows_the_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = repo("gta-sparse-set");
	let w = work.to_str().unwrap();

	gta(w, &["sparse-checkout", "set", "a"], b"");

	let t = git(w, &["ls-files", "-t"]);
	assert!(t.contains("H a/f"), "in-cone file present: {t}");
	assert!(t.contains("H root.txt"), "root file present: {t}");
	assert!(t.contains("S b/h"), "out-of-cone file skip-worktree: {t}");
	assert!(!work.join("b/h").exists(), "out-of-cone file removed");

	std::fs::remove_dir_all(&work).ok();
}

/// git rejects a cone argument that names a tracked *file* (a cone set takes directories); there is no
/// `--skip-checks` in gta.
#[test]
fn cone_set_refuses_a_tracked_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo("gta-sparse-file");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["sparse-checkout", "set", "a/f"]);
	assert!(err.contains("not a directory"), "stderr: {err}");
	// Nothing was applied: the pattern file was not written.
	assert!(!work.join(".git/info/sparse-checkout").exists());

	std::fs::remove_dir_all(&work).ok();
}

/// git rejects a leading slash in a cone argument (it is a directory, not a root-anchored pattern).
#[test]
fn cone_set_refuses_a_leading_slash() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo("gta-sparse-slash");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["sparse-checkout", "set", "/a"]);
	assert!(err.contains("no leading slash"), "stderr: {err}");

	std::fs::remove_dir_all(&work).ok();
}

/// git refuses a non-cone `set`/`add` from a subdirectory (non-cone patterns are root-relative, so a
/// subdirectory invocation is ambiguous); the same command from the toplevel succeeds.
#[test]
fn noncone_set_requires_the_toplevel() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo("gta-sparse-noncone-subdir");
	let w = work.to_str().unwrap();
	let subdir = work.join("a");
	let sub = subdir.to_str().unwrap();

	let err = gta_fail(sub, &["sparse-checkout", "set", "--no-cone", "/b/"]);
	assert!(err.contains("toplevel"), "stderr: {err}");

	// From the toplevel the same set is accepted.
	gta(w, &["sparse-checkout", "set", "--no-cone", "/b/"], b"");
	assert!(work.join(".git/info/sparse-checkout").exists());

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
		let probe = unique_tmp("probe-sparse");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
