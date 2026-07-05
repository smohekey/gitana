//! `gta show` end-to-end: a commit's header and diff, a blob's raw bytes (cross-checked against
//! git), and a tag and tree. Output is gta's own porcelain form, so most assertions are
//! structural rather than byte-for-byte against `git show`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A repo with two commits to `a.txt`: "line1\nline2\n", then "line1\nCHANGED\nline2\n". Returns
/// the work dir and the first commit id.
fn two_commits(tag: &str) -> (PathBuf, String) {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(w, &["init", "--object-format=sha256", "-q", "."]);

	std::fs::write(work.join("a.txt"), b"line1\nline2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "first commit");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"line1\nCHANGED\nline2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "second commit");

	(work, c1)
}

#[test]
fn show_commit_displays_header_and_diff() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let (work, _c1) = two_commits("gta-show-commit");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	let out = gta(w, &["show", "HEAD"], b"");
	assert!(out.contains(&format!("commit {head}")), "{out}");
	assert!(out.contains("Author: T <t@e>"), "{out}");
	assert!(out.contains("Date:   "), "{out}");
	assert!(out.contains("    second commit"), "{out}");
	// The diff against the first parent, in gta's unified form.
	assert!(out.contains("diff --git a/a.txt b/a.txt"), "{out}");
	assert!(out.contains("+CHANGED"), "{out}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_root_commit_diffs_against_the_empty_tree() {
	if !git_supports_sha256() {
		return;
	}
	let (work, c1) = two_commits("gta-show-root");
	let w = work.to_str().unwrap();

	let out = gta(w, &["show", &c1], b"");
	// A root commit adds every file (old side is /dev/null).
	assert!(out.contains("--- /dev/null"), "{out}");
	assert!(out.contains("+++ b/a.txt"), "{out}");
	assert!(out.contains("+line1") && out.contains("+line2"), "{out}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_blob_outputs_raw_content() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-blob");
	let w = work.to_str().unwrap();

	// `<rev>:<path>` resolves to the blob and shows its raw bytes.
	assert_eq!(
		gta(w, &["show", "HEAD:a.txt"], b""),
		"line1\nCHANGED\nline2\n"
	);
	// ...the same bytes git's blob carries.
	let blob = git(w, &["rev-parse", "HEAD:a.txt"]).trim().to_owned();
	assert_eq!(
		gta(w, &["show", &blob], b""),
		git(w, &["cat-file", "-p", &blob])
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_rev_path_resolves_a_subtree() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-show-subtree");
	let w = work.to_str().unwrap();
	git(w, &["init", "--object-format=sha256", "-q", "."]);
	std::fs::create_dir(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/x.txt"), b"X\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// `<rev>:<dir>` resolves to the subtree; `<rev>:<dir>/<file>` to the blob within it.
	let out = gta(w, &["show", "HEAD:dir"], b"");
	assert!(out.contains("tree "), "{out}");
	assert!(out.lines().any(|l| l == "x.txt"), "{out}");
	assert_eq!(gta(w, &["show", "HEAD:dir/x.txt"], b""), "X\n");

	// A trailing slash requires a directory: it is fine on `dir`, an error on the blob.
	assert!(gta(w, &["show", "HEAD:dir/"], b"").contains("tree "));
	let err = gta_fail(w, &["show", "HEAD:dir/x.txt/"]);
	assert!(err.contains("not a directory"), "stderr: {err}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_index_path_shows_the_staged_blob() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-index");
	let w = work.to_str().unwrap();

	// Stage a change without committing: the index now differs from HEAD.
	std::fs::write(work.join("a.txt"), b"STAGED\n").unwrap();
	git(w, &["add", "a.txt"]);

	// `:a.txt` (and the explicit `:0:a.txt`) show the staged blob, not HEAD's.
	assert_eq!(gta(w, &["show", ":a.txt"], b""), "STAGED\n");
	assert_eq!(gta(w, &["show", ":0:a.txt"], b""), "STAGED\n");
	assert_eq!(
		gta(w, &["show", "HEAD:a.txt"], b""),
		"line1\nCHANGED\nline2\n"
	);
	// The same form works for plumbing too.
	assert_eq!(gta(w, &["cat-file", "-p", ":a.txt"], b""), "STAGED\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_tree_lists_entries() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-tree");
	let w = work.to_str().unwrap();
	let tree = git(w, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();

	let out = gta(w, &["show", &tree], b"");
	assert!(out.contains(&format!("tree {tree}")), "{out}");
	assert!(out.lines().any(|l| l == "a.txt"), "{out}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_tag_displays_tag_then_target() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-tag");
	let w = work.to_str().unwrap();
	git(
		w,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"tag",
			"-a",
			"v1",
			"-m",
			"release one",
		],
	);

	let out = gta(w, &["show", "v1"], b"");
	assert!(out.contains("tag v1"), "{out}");
	assert!(out.contains("Tagger: T <t@e>"), "{out}");
	assert!(out.contains("release one"), "{out}");
	// ...followed by the commit the tag points at.
	assert!(out.contains("commit "), "{out}");
	assert!(out.contains("+CHANGED"), "{out}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_signed_tag_surfaces_the_signature_block() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-signed-tag");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// Hand-craft a signed annotated tag object: git appends the armor block after the message. A
	// real signature is unnecessary here — we only assert the block is surfaced, not verified.
	let payload = format!(
		"object {head}\ntype commit\ntag v1s\ntagger T <t@e> 1700000000 +0000\n\n\
		 signed release\n-----BEGIN SSH SIGNATURE-----\nZmFrZXNpZw==\n-----END SSH SIGNATURE-----\n"
	);
	let id = git_stdin(
		w,
		&["hash-object", "-t", "tag", "-w", "--stdin"],
		payload.as_bytes(),
	);
	git(w, &["update-ref", "refs/tags/v1s", id.trim()]);

	let out = gta(w, &["show", "v1s"], b"");
	assert!(out.contains("tag v1s"), "{out}");
	assert!(out.contains("signed release"), "{out}");
	assert!(
		out.contains("-----BEGIN SSH SIGNATURE-----") && out.contains("-----END SSH SIGNATURE-----"),
		"the armor block must be surfaced, not dropped: {out}"
	);
	// ...still followed by the tagged commit.
	assert!(out.contains("commit "), "{out}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn show_and_cat_file_work_in_a_bare_repo() {
	if !git_supports_sha256() {
		return;
	}
	let bare = unique_tmp("gta-show-bare.git");
	let b = bare.to_str().unwrap();
	git(b, &["init", "--bare", "--object-format=sha256", "-q", "."]);

	// Hash a blob straight into the bare object store (no work tree involved).
	let content = bare.parent().unwrap().join("content");
	std::fs::write(&content, b"hello\n").unwrap();
	let blob = git(b, &["hash-object", "-w", content.to_str().unwrap()])
		.trim()
		.to_owned();

	// Object-only lookups need no work tree.
	assert_eq!(gta(b, &["show", &blob], b""), "hello\n");
	assert_eq!(gta(b, &["cat-file", "-p", &blob], b""), "hello\n");
	// A work-tree command fails with a clear message rather than "not a repository".
	let err = gta_fail(b, &["status"]);
	assert!(err.contains("work tree"), "stderr: {err}");

	let _ = std::fs::remove_file(&content);
	std::fs::remove_dir_all(&bare).ok();
}

#[test]
fn show_defaults_to_head() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _c1) = two_commits("gta-show-default");
	let w = work.to_str().unwrap();

	assert_eq!(gta(w, &["show"], b""), gta(w, &["show", "HEAD"], b""));

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

fn git_stdin(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let mut child = Command::new("git")
		.args(&full)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn git");
	child
		.stdin
		.take()
		.expect("stdin")
		.write_all(stdin)
		.expect("write stdin");
	let out = child.wait_with_output().expect("run git");
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
		let probe = unique_tmp("probe-show");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
