//! `gta checkout` end-to-end: branch switching still works, and path restore (from the
//! index or a tree-ish) restores files without moving `HEAD`, cross-checked against real git.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn checkout_restores_paths_without_moving_head() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-checkout-restore");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");

	// `checkout -- <path>` restores the working tree from the index, discarding edits.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	gta(w, &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	// `checkout <tree-ish> -- <path>` restores both the working tree and the index.
	gta(w, &["checkout", &c1, "--", "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert_eq!(
		git(w, &["diff", "--cached", "--name-only"]).trim(),
		"a.txt",
		"the tree content is staged"
	);

	// HEAD never moved during path restore.
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn checkout_without_paths_still_switches_branches() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-switch");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	gta(w, &["branch", "feature"], b"");
	gta(w, &["checkout", "feature"], b"");
	assert_eq!(
		git(w, &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/feature"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn checkout_restore_is_relative_to_cwd() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-subdir");
	let w = work.to_str().unwrap();
	let sub = work.join("sub");
	let s = sub.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"ROOT\n").unwrap();
	std::fs::create_dir_all(&sub).unwrap();
	std::fs::write(sub.join("a.txt"), b"SUB\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// `gta -C sub checkout -- a.txt` restores sub/a.txt, leaving the root file dirty,
	// matching `git -C sub checkout -- a.txt`.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	gta(s, &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	// `gta -C sub checkout -- .` restores only entries under sub/, like git does.
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	gta(s, &["checkout", "--", "."], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
#[cfg(unix)]
fn checkout_restore_resolves_symlinked_cwd() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-symlink");
	let w = work.to_str().unwrap();
	let sub = work.join("sub");
	gta(w, &["init"], b"");

	std::fs::create_dir_all(&sub).unwrap();
	std::fs::write(sub.join("a.txt"), b"SUB\n").unwrap();
	std::os::unix::fs::symlink("sub", work.join("linksub")).unwrap();
	git(w, &["add", "sub"]);
	commit(w, "one");

	// `-C linksub` (a symlink to `sub`) must resolve to `sub`, so `a.txt` means `sub/a.txt`.
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	let link = work.join("linksub");
	gta(link.to_str().unwrap(), &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	// The tracked path is `sub/a.txt`, never `linksub/a.txt`.
	assert!(
		git(w, &["ls-files"])
			.lines()
			.all(|l| !l.starts_with("linksub/"))
	);

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
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("gta-probe");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
