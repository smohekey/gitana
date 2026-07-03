//! Differential test: bitmaps [`gitana_object::build_reachability_bitmaps`] computes from our own
//! reachability walk, serialized with our writer, are accepted and trusted by stock git.
//!
//! We let git write a MIDX (for its reverse index), build our own reachability + type bitmaps over
//! every commit, write the `.bitmap`, and confirm `git rev-list --test-bitmap` (which verifies each
//! entry against a real history walk) and `git multi-pack-index verify` pass.
//!
//! Uses a SHA-1 repo (git's default); skips when git is too old for MIDX bitmaps (< 2.34).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_object::{
	ObjectId, ObjectKind, Sha1, build_reachability_bitmaps, decode_multi_pack_index,
};
use tempfile::TempDir;

#[test]
fn git_accepts_bitmaps_we_build() {
	let repo = TempDir::new().expect("tempdir");
	let dir = repo.path();

	if !try_git(dir, &["init", "-q", "--object-format=sha1", "."]) {
		run_git(dir, &["init", "-q", "."]);
	}
	run_git(dir, &["config", "user.email", "t@example.com"]);
	run_git(dir, &["config", "user.name", "Tester"]);
	run_git(dir, &["config", "commit.gpgSign", "false"]);
	for n in 1..=5 {
		std::fs::write(dir.join(format!("f{n}.txt")), format!("a{n}\n")).expect("write");
		std::fs::write(dir.join("shared.txt"), format!("s{n}\n")).expect("write shared");
		run_git(dir, &["add", "-A"]);
		run_git(dir, &["commit", "-q", "-m", &format!("c{n}")]);
	}
	run_git(dir, &["repack", "-q", "-d"]);

	// git writes the MIDX (with the reverse index) and its own bitmap; we overwrite the bitmap.
	if !try_git(dir, &["multi-pack-index", "write", "--bitmap"]) {
		eprintln!("skipping: git without MIDX bitmap support");
		return;
	}
	let pack_dir = dir.join(".git/objects/pack");
	let midx_bytes = std::fs::read(pack_dir.join("multi-pack-index")).expect("read MIDX");
	let midx = decode_multi_pack_index::<Sha1>(&midx_bytes).expect("decode MIDX");
	if midx.reverse_index().is_none() {
		eprintln!("skipping: git wrote no RIDX chunk");
		return;
	}

	// Read every object once (kind + bytes) so the builder's readers are plain map lookups.
	let store = read_all_objects(dir);
	// Bitmap every commit reachable from all refs.
	let selected: Vec<ObjectId<Sha1>> = git_stdout(dir, &["rev-list", "--all"])
		.lines()
		.map(|line| ObjectId::<Sha1>::from_hex(line.trim()).expect("commit id"))
		.collect();
	assert!(!selected.is_empty());

	let built = build_reachability_bitmaps(
		&midx,
		&selected,
		|id| store.get(id).map(|(kind, _)| *kind),
		|id| store.get(id).map(|(_, data)| data.clone()),
	)
	.expect("build bitmaps");
	let ours = built.encode::<Sha1>(midx.checksum()).expect("encode");

	// Replace git's bitmap with ours (git writes it read-only).
	let bitmap_path = find_bitmap(&pack_dir);
	let mut perms = std::fs::metadata(&bitmap_path).unwrap().permissions();
	#[allow(clippy::permissions_set_readonly_false)]
	perms.set_readonly(false);
	std::fs::set_permissions(&bitmap_path, perms).expect("clear read-only");
	std::fs::write(&bitmap_path, &ours).expect("write our bitmap");

	assert!(
		try_git(dir, &["rev-list", "--test-bitmap", "HEAD"]),
		"git rev-list --test-bitmap accepts bitmaps we built",
	);
	assert!(
		try_git(dir, &["multi-pack-index", "verify"]),
		"git multi-pack-index verify accepts bitmaps we built",
	);
}

/// Every object in the repo, as `id -> (kind, payload)`, via `git cat-file --batch`.
fn read_all_objects(dir: &Path) -> HashMap<ObjectId<Sha1>, (ObjectKind, Vec<u8>)> {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(["cat-file", "--batch-all-objects", "--batch", "--buffer"])
		.output()
		.expect("git cat-file");
	assert!(output.status.success(), "git cat-file failed");

	let mut store = HashMap::new();
	let bytes = output.stdout;
	let mut i = 0;
	while i < bytes.len() {
		// Header line: "<sha> <type> <size>\n".
		let nl = i
			+ bytes[i..]
				.iter()
				.position(|&b| b == b'\n')
				.expect("header newline");
		let header = std::str::from_utf8(&bytes[i..nl]).expect("utf8 header");
		let mut parts = header.split(' ');
		let id = ObjectId::<Sha1>::from_hex(parts.next().unwrap()).expect("id");
		let kind = match parts.next().unwrap() {
			"commit" => ObjectKind::Commit,
			"tree" => ObjectKind::Tree,
			"blob" => ObjectKind::Blob,
			"tag" => ObjectKind::Tag,
			other => panic!("unexpected object type {other}"),
		};
		let size: usize = parts.next().unwrap().parse().expect("size");
		let data_start = nl + 1;
		let data = bytes[data_start..data_start + size].to_vec();
		store.insert(id, (kind, data));
		i = data_start + size + 1; // skip the trailing newline
	}
	store
}

fn find_bitmap(pack_dir: &Path) -> PathBuf {
	std::fs::read_dir(pack_dir)
		.expect("read pack dir")
		.filter_map(|e| e.ok().map(|e| e.path()))
		.find(|p| p.extension().and_then(|x| x.to_str()) == Some("bitmap"))
		.expect("a .bitmap exists")
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git");
	assert!(output.status.success(), "git {args:?} failed");
	String::from_utf8(output.stdout).expect("utf8")
}

fn run_git(dir: &Path, args: &[&str]) {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git");
	assert!(
		output.status.success(),
		"git {args:?} failed:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

fn try_git(dir: &Path, args: &[&str]) -> bool {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}
