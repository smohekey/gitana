//! Differential test: [`gitana_object::decode_ewah`] reads the EWAH streams stock `git repack
//! --write-bitmap-index` produces. A pack `.bitmap` begins with four type bitmaps (commits, trees,
//! blobs, tags) that partition every object in the pack, so decoding them and checking the union
//! covers `[0, object_count)` exactly once exercises the decoder against real git output.
//!
//! Uses a SHA-1 repo (git's default, always bitmap-capable), since EWAH is hash-independent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_object::{Sha1, decode_ewah, decode_pack_index};
use tempfile::TempDir;

/// Bytes of a pack `.bitmap` header before the first EWAH stream: `BITM`, u16 version, u16 flags,
/// u32 entry count, then the pack checksum (`H::RAW_LEN`, 20 for SHA-1).
const BITMAP_HEADER_LEN: usize = 4 + 2 + 2 + 4 + 20;

#[test]
fn we_read_git_bitmap_type_indexes() {
	let repo = TempDir::new().expect("tempdir");
	let dir = repo.path();
	let path = |p: &str| dir.join(p).to_str().unwrap().to_owned();

	// Pin SHA-1 so the 20-byte header width holds even under GIT_DEFAULT_HASH=sha256. `--object-format`
	// arrived with SHA-256 support (git 2.29); on older git the flag is absent but sha1 is the only
	// format, so plain `git init` suffices — fall back to it when the flagged form is unsupported.
	if !try_git(&["init", "-q", "--object-format=sha1", &path("")]) {
		run_git(&["init", "-q", &path("")]);
	}
	run_git(&["-C", &path(""), "config", "user.email", "t@example.com"]);
	run_git(&["-C", &path(""), "config", "user.name", "Tester"]);
	// Stay hermetic against an inherited global commit.gpgSign=true (gpg may be absent).
	run_git(&["-C", &path(""), "config", "commit.gpgSign", "false"]);

	// A few commits so the pack holds commits, trees, and blobs (no tags — that type index is empty).
	for (n, body) in [("1", "alpha\n"), ("2", "beta\n"), ("3", "gamma\n")] {
		std::fs::write(dir.join(format!("file{n}.txt")), body).expect("write file");
		std::fs::write(dir.join("shared.txt"), format!("rev {n}\n")).expect("write shared");
		run_git(&["-C", &path(""), "add", "-A"]);
		run_git(&[
			"-C",
			&path(""),
			"commit",
			"-q",
			"-m",
			&format!("commit {n}"),
		]);
	}

	// One pack with a bitmap over every object.
	run_git(&[
		"-C",
		&path(""),
		"repack",
		"-a",
		"-d",
		"--write-bitmap-index",
	]);

	let pack_dir = dir.join(".git/objects/pack");
	let bitmap = std::fs::read(find_by_ext(&pack_dir, "bitmap")).expect("read .bitmap");
	let idx = std::fs::read(find_by_ext(&pack_dir, "idx")).expect("read .idx");
	let index = decode_pack_index::<Sha1>(&idx).expect("decode git's .idx");
	let object_count = index.len() as u32;

	assert_eq!(&bitmap[0..4], b"BITM", "bitmap signature");
	assert_eq!(
		u16::from_be_bytes([bitmap[4], bitmap[5]]),
		1,
		"bitmap version 1"
	);
	// The header records the checksum of the pack this bitmap is for.
	assert_eq!(
		&bitmap[12..BITMAP_HEADER_LEN],
		index.pack_checksum(),
		"bitmap header names its pack",
	);

	// The four type indexes, in order: commits, trees, blobs, tags.
	let mut cursor = BITMAP_HEADER_LEN;
	let mut union: HashSet<u32> = HashSet::new();
	let mut total = 0u64;
	for type_name in ["commits", "trees", "blobs", "tags"] {
		let (type_bitmap, consumed) =
			decode_ewah(&bitmap[cursor..]).unwrap_or_else(|e| panic!("decode {type_name}: {e}"));
		cursor += consumed;
		for pos in type_bitmap.set_bits() {
			assert!(
				pos < object_count,
				"{type_name} position {pos} within the pack"
			);
			assert!(
				union.insert(pos),
				"position {pos} claimed by two type indexes"
			);
		}
		total += type_bitmap.count();
	}

	// The four type indexes partition every object in the pack, exactly once.
	assert_eq!(
		total, object_count as u64,
		"type indexes cover every object once"
	);
	assert_eq!(
		union.len(),
		object_count as usize,
		"union is every object position"
	);
}

/// The single file in `dir` with the given extension.
fn find_by_ext(dir: &Path, ext: &str) -> PathBuf {
	let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
		.expect("read pack dir")
		.filter_map(|e| e.ok().map(|e| e.path()))
		.filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
		.collect();
	assert_eq!(found.len(), 1, "exactly one .{ext} in {dir:?}: {found:?}");
	found.pop().unwrap()
}

fn run_git(args: &[&str]) {
	let output = Command::new("git").args(args).output().expect("run git");
	assert!(
		output.status.success(),
		"git {args:?} failed:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

/// Run git, returning whether it succeeded (for optional flags older git may not support).
fn try_git(args: &[&str]) -> bool {
	Command::new("git")
		.args(args)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}
