//! Differential test: [`gitana_object::encode_midx_bitmap`] produces a `.bitmap` stock git accepts.
//!
//! We let git write a MIDX reachability bitmap, decode its type indexes and per-commit reachability
//! with our reader, re-serialize them with our writer, and (a) confirm the round-trip is faithful,
//! then (b) overwrite git's `.bitmap` with ours and confirm `git rev-list --test-bitmap` and
//! `git multi-pack-index verify` still pass — i.e. git reads and trusts what we wrote.
//!
//! Uses a SHA-1 repo (git's default); skips when git is too old for MIDX bitmaps (< 2.34).

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_object::{
	EwahBitmap, ObjectKind, Sha1, decode_midx_bitmap, decode_multi_pack_index, encode_midx_bitmap,
};
use tempfile::TempDir;

#[test]
fn git_accepts_a_bitmap_we_serialize() {
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

	let bitmap_path = find_bitmap(&pack_dir);
	let git_index = decode_midx_bitmap::<Sha1>(&std::fs::read(&bitmap_path).expect("read .bitmap"))
		.expect("decode");

	// Re-serialize git's own type indexes and per-commit reachability with our writer.
	let type_bitmaps = [
		git_index.type_bitmap(ObjectKind::Commit),
		git_index.type_bitmap(ObjectKind::Tree),
		git_index.type_bitmap(ObjectKind::Blob),
		git_index.type_bitmap(ObjectKind::Tag),
	];
	let commits: Vec<(u32, &EwahBitmap)> = git_index
		.bitmapped_commit_positions()
		.map(|pos| {
			(
				pos,
				git_index.commit_reachability(pos).expect("reachability"),
			)
		})
		.collect();
	let ours = encode_midx_bitmap::<Sha1>(midx.checksum(), type_bitmaps, &commits).expect("encode");

	// (a) The round-trip through our reader is faithful.
	let reparsed = decode_midx_bitmap::<Sha1>(&ours).expect("decode our bitmap");
	assert_eq!(reparsed.midx_checksum(), midx.checksum());
	for kind in [
		ObjectKind::Commit,
		ObjectKind::Tree,
		ObjectKind::Blob,
		ObjectKind::Tag,
	] {
		assert_eq!(
			reparsed.type_bitmap(kind).set_bits().collect::<Vec<_>>(),
			git_index.type_bitmap(kind).set_bits().collect::<Vec<_>>(),
			"type index {kind:?} survives the round-trip",
		);
	}
	for (pos, reach) in &commits {
		assert_eq!(
			reparsed.commit_reachability(*pos),
			Some(*reach),
			"commit {pos}"
		);
	}

	// (b) git reads and trusts our bitmap in place of its own. git writes `.bitmap` read-only, so
	// clear that bit before replacing it (on Windows a read-only file cannot be removed or written).
	let mut perms = std::fs::metadata(&bitmap_path).unwrap().permissions();
	// A world-writable temp file in a test is fine; we just need to overwrite it.
	#[allow(clippy::permissions_set_readonly_false)]
	perms.set_readonly(false);
	std::fs::set_permissions(&bitmap_path, perms).expect("clear read-only");
	std::fs::write(&bitmap_path, &ours).expect("write our bitmap");

	assert!(
		try_git(dir, &["rev-list", "--test-bitmap", "HEAD"]),
		"git rev-list --test-bitmap accepts our bitmap",
	);
	assert!(
		try_git(dir, &["multi-pack-index", "verify"]),
		"git multi-pack-index verify accepts our bitmap",
	);
}

fn find_bitmap(pack_dir: &Path) -> PathBuf {
	std::fs::read_dir(pack_dir)
		.expect("read pack dir")
		.filter_map(|e| e.ok().map(|e| e.path()))
		.find(|p| p.extension().and_then(|x| x.to_str()) == Some("bitmap"))
		.expect("a .bitmap exists")
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
