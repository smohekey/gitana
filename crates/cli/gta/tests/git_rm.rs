//! `gta rm` end-to-end: removes tracked paths from the index and (unless `--cached`) the working
//! tree, enforces git's data-safety check with its `-f` override, requires `-r` for directories,
//! and supports `--dry-run` — all cross-checked against real git's view of the result.

use std::path::PathBuf;
use std::process::Command;

/// A repo with two committed files: `a.txt`=A and `dir/b.txt`=B. Returns the work dir.
fn repo_with_files(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/b.txt"), b"B\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	work
}

#[test]
fn rm_removes_from_index_and_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = repo_with_files("gta-rm-basic");
	let w = work.to_str().unwrap();

	assert_eq!(gta(w, &["rm", "a.txt"], b""), "rm 'a.txt'\n");
	assert_eq!(git(w, &["ls-files"]).trim(), "dir/b.txt");
	assert!(!work.join("a.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_cached_keeps_worktree_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-cached");
	let w = work.to_str().unwrap();

	gta(w, &["rm", "--cached", "a.txt"], b"");
	// Dropped from the index but left in the working tree (now untracked).
	assert_eq!(git(w, &["ls-files"]).trim(), "dir/b.txt");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_refuses_local_modifications() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-local");
	let w = work.to_str().unwrap();

	std::fs::write(work.join("a.txt"), b"DIRTY\n").unwrap();
	let err = gta_fail(w, &["rm", "a.txt"]);
	assert!(err.contains("local modifications"), "stderr: {err}");
	// Nothing removed.
	assert!(git(w, &["ls-files"]).contains("a.txt"));
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"DIRTY\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_refuses_staged_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-staged");
	let w = work.to_str().unwrap();

	std::fs::write(work.join("a.txt"), b"STAGED\n").unwrap();
	git(w, &["add", "a.txt"]);
	let err = gta_fail(w, &["rm", "a.txt"]);
	assert!(err.contains("changes staged in the index"), "stderr: {err}");
	assert!(git(w, &["ls-files"]).contains("a.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_refuses_staged_and_local_even_cached() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-both");
	let w = work.to_str().unwrap();

	// Index differs from HEAD (staged) and the working tree differs from the index (local).
	std::fs::write(work.join("a.txt"), b"S1\n").unwrap();
	git(w, &["add", "a.txt"]);
	std::fs::write(work.join("a.txt"), b"S2\n").unwrap();

	for args in [&["rm", "a.txt"][..], &["rm", "--cached", "a.txt"][..]] {
		let err = gta_fail(w, args);
		assert!(
			err.contains("different from both the file and the HEAD"),
			"stderr: {err}"
		);
	}
	assert!(git(w, &["ls-files"]).contains("a.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_force_overrides_safety() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-force");
	let w = work.to_str().unwrap();

	std::fs::write(work.join("a.txt"), b"DIRTY\n").unwrap();
	gta(w, &["rm", "-f", "a.txt"], b"");
	assert_eq!(git(w, &["ls-files"]).trim(), "dir/b.txt");
	assert!(!work.join("a.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_cached_allows_staged_only_change() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-cached-staged");
	let w = work.to_str().unwrap();

	std::fs::write(work.join("a.txt"), b"STAGED\n").unwrap();
	git(w, &["add", "a.txt"]);
	// `--cached` keeps the file, so a staged-only change is recoverable: git permits it.
	gta(w, &["rm", "--cached", "a.txt"], b"");
	assert_eq!(git(w, &["ls-files"]).trim(), "dir/b.txt");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"STAGED\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_directory_requires_recursive() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-dir");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["rm", "dir"]);
	assert!(err.contains("without -r"), "stderr: {err}");
	assert!(git(w, &["ls-files"]).contains("dir/b.txt"));

	gta(w, &["rm", "-r", "dir"], b"");
	assert_eq!(git(w, &["ls-files"]).trim(), "a.txt");
	assert!(!work.join("dir").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_unmatched_and_untracked_pathspecs_error() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-nomatch");
	let w = work.to_str().unwrap();

	let err = gta_fail(w, &["rm", "missing.txt"]);
	assert!(err.contains("did not match"), "stderr: {err}");

	// An untracked file is not a tracked path, so `rm` refuses it too.
	std::fs::write(work.join("u.txt"), b"U\n").unwrap();
	let err = gta_fail(w, &["rm", "u.txt"]);
	assert!(err.contains("did not match"), "stderr: {err}");
	assert!(work.join("u.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_dry_run_changes_nothing() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-dryrun");
	let w = work.to_str().unwrap();

	assert_eq!(gta(w, &["rm", "-n", "a.txt"], b""), "rm 'a.txt'\n");
	// Reported, but neither the index nor the working tree changed.
	assert!(git(w, &["ls-files"]).contains("a.txt"));
	assert!(work.join("a.txt").exists());

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_refuses_to_remove_a_path_now_occupied_by_a_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-typechange");
	let w = work.to_str().unwrap();

	// Replace the tracked file `a.txt` with a directory of the same name on disk.
	std::fs::remove_file(work.join("a.txt")).unwrap();
	std::fs::create_dir(work.join("a.txt")).unwrap();
	std::fs::write(work.join("a.txt/inner"), b"x\n").unwrap();

	// Even with -f, removal fails and the index entry is kept (git exits non-zero here too) —
	// rather than reporting success while leaving the directory behind.
	let err = gta_fail(w, &["rm", "-f", "a.txt"]);
	assert!(!err.is_empty());
	assert!(
		git(w, &["ls-files"]).contains("a.txt"),
		"a.txt stays tracked"
	);
	assert!(
		work.join("a.txt").is_dir(),
		"the directory is left in place"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_multipath_failure_removes_what_it_can() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-rm-partial");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	std::fs::write(work.join("c.txt"), b"C\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// Make the second path un-removable: a directory now occupies `c.txt`.
	std::fs::remove_file(work.join("c.txt")).unwrap();
	std::fs::create_dir(work.join("c.txt")).unwrap();

	// Per-path, like git: `a.txt` (removable) is dropped from the working tree and the index;
	// `c.txt` (un-removable) is kept in both. The command exits non-zero but still reports the
	// removal that did happen on stdout, rather than hiding the side effect behind the error.
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "rm", "-f", "a.txt", "c.txt"])
		.output()
		.unwrap();
	assert!(!out.status.success());
	assert_eq!(
		String::from_utf8(out.stdout).unwrap(),
		"rm 'a.txt'\n",
		"the successful removal is reported"
	);

	assert!(!work.join("a.txt").exists(), "a.txt is removed from disk");
	let tracked = git(w, &["ls-files"]);
	assert!(!tracked.contains("a.txt"), "a.txt dropped from the index");
	assert!(tracked.contains("c.txt"), "c.txt kept (its removal failed)");
	assert!(
		work.join("c.txt").is_dir(),
		"the directory is left in place"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn rm_aborts_before_deleting_when_index_is_locked() {
	if !git_supports_sha256() {
		return;
	}
	let work = repo_with_files("gta-rm-locked");
	let w = work.to_str().unwrap();

	// Simulate another process holding the index lock.
	std::fs::write(work.join(".git/index.lock"), b"").unwrap();

	let err = gta_fail(w, &["rm", "a.txt"]);
	assert!(err.contains("locked"), "stderr: {err}");
	// The command failed before mutating anything: the file is neither deleted nor untracked.
	assert!(
		work.join("a.txt").exists(),
		"a.txt must not be deleted when the index is locked"
	);
	assert!(git(w, &["ls-files"]).contains("a.txt"));

	let _ = std::fs::remove_file(work.join(".git/index.lock"));
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
		let probe = unique_tmp("probe-rm");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
