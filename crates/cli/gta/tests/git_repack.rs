//! `gta repack` end-to-end: consolidating loose objects and existing packs into a single pack,
//! cross-checked with stock git — every object survives (`git fsck` is clean and the full object
//! set is unchanged) and the pack gta wrote is readable by git.

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn repack_consolidates_loose_objects_and_git_reads_the_result() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-repack");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	// Commits (via stock git) leave loose objects and no pack.
	write_add_commit(w, &work, "a.txt", "alpha", "one");
	write_add_commit(w, &work, "b.txt", "beta", "two");
	write_add_commit(w, &work, "c.txt", "gamma", "three");
	assert!(
		has_loose_objects(&work),
		"commits should leave loose objects"
	);
	assert_eq!(pack_files(&work).len(), 0, "no pack before repack");
	let before = all_object_ids(w);

	gta(w, &["repack"], b"");

	// Exactly one pack (+ its .idx), and no loose objects remain.
	let packs = pack_files(&work);
	assert_eq!(
		packs.iter().filter(|p| p.ends_with(".pack")).count(),
		1,
		"one pack after repack: {packs:?}"
	);
	assert_eq!(packs.iter().filter(|p| p.ends_with(".idx")).count(), 1);
	assert!(!has_loose_objects(&work), "loose objects removed by repack");

	// Stock git reads gta's repacked repo: fsck is clean, the object set is unchanged, and
	// history still resolves.
	assert!(git_ok(w, &["fsck", "--full", "--strict"]), "git fsck");
	assert_eq!(all_object_ids(w), before, "every object preserved");
	assert!(git_ok(w, &["rev-list", "--all"]), "git rev-list --all");
	git_id(w, &["cat-file", "-p", "HEAD"]);

	// Idempotent: a second repack is a no-op and leaves the single pack in place.
	let out = gta(w, &["repack"], b"");
	assert!(out.contains("Nothing to repack"), "second repack: {out}");
	assert_eq!(
		pack_files(&work)
			.iter()
			.filter(|p| p.ends_with(".pack"))
			.count(),
		1
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn repack_merges_an_existing_pack_with_new_loose_objects() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-repack-multi");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	write_add_commit(w, &work, "a.txt", "alpha", "one");
	gta(w, &["repack"], b""); // first pack
	assert_eq!(
		pack_files(&work)
			.iter()
			.filter(|p| p.ends_with(".pack"))
			.count(),
		1
	);

	// New commits add loose objects on top of the existing pack.
	write_add_commit(w, &work, "b.txt", "beta", "two");
	assert!(has_loose_objects(&work));
	let before = all_object_ids(w);

	// Repack consolidates the old pack + the new loose objects into a single pack.
	gta(w, &["repack"], b"");
	assert_eq!(
		pack_files(&work)
			.iter()
			.filter(|p| p.ends_with(".pack"))
			.count(),
		1,
		"one pack after consolidating"
	);
	assert!(!has_loose_objects(&work));
	assert!(git_ok(w, &["fsck", "--full", "--strict"]));
	assert_eq!(all_object_ids(w), before, "every object preserved");

	std::fs::remove_dir_all(&work).ok();
}

// --- helpers -------------------------------------------------------------------------------

/// Whether any loose object (`.git/objects/<aa>/…`) exists in the repo at `work`.
fn has_loose_objects(work: &Path) -> bool {
	let objects = work.join(".git/objects");
	let Ok(entries) = std::fs::read_dir(&objects) else {
		return false;
	};
	entries.filter_map(|e| e.ok()).any(|entry| {
		let name = entry.file_name();
		let name = name.to_string_lossy();
		name.len() == 2
			&& name.bytes().all(|b| b.is_ascii_hexdigit())
			&& std::fs::read_dir(entry.path())
				.map(|mut d| d.next().is_some())
				.unwrap_or(false)
	})
}

/// The file names under `.git/objects/pack/` (`.pack` and `.idx`); empty if the dir is absent.
fn pack_files(work: &Path) -> Vec<String> {
	let pack = work.join(".git/objects/pack");
	let Ok(entries) = std::fs::read_dir(&pack) else {
		return Vec::new();
	};
	entries
		.filter_map(|e| e.ok())
		.map(|e| e.file_name().to_string_lossy().into_owned())
		.collect()
}

/// Every object id git can see in the repo (loose or packed), sorted — the full stored set.
fn all_object_ids(w: &str) -> Vec<String> {
	let mut ids: Vec<String> = git(
		w,
		&[
			"cat-file",
			"--batch-check=%(objectname)",
			"--batch-all-objects",
			"--unordered",
		],
	)
	.lines()
	.map(str::to_owned)
	.collect();
	ids.sort();
	ids
}

fn write_add_commit(w: &str, work: &Path, file: &str, content: &str, msg: &str) {
	std::fs::write(work.join(file), format!("{content}\n")).unwrap();
	git(w, &["add", "."]);
	git_id(w, &["commit", "-q", "-m", msg]);
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

/// `git` with a fixed identity, for commits.
fn git_id(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir, "-c", "user.name=T", "-c", "user.email=t@e"];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn git_ok(dir: &str, args: &[&str]) -> bool {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	Command::new("git")
		.args(&full)
		.output()
		.expect("run git")
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
		let probe = unique_tmp("probe-repack");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
