//! Differential test: [`gitana_object::decode_multi_pack_index`] reads the `RIDX` reverse-index
//! chunk that stock `git multi-pack-index write --bitmap` records, and [`gitana_object::pack_order`]
//! reproduces git's bitmap object order exactly.
//!
//! Uses a SHA-1 repo (git's default), and skips when git is too old for MIDX bitmaps (< 2.34) or
//! does not emit an `RIDX` chunk.

use std::path::Path;
use std::process::Command;

use gitana_object::{Sha1, decode_multi_pack_index, pack_order};
use tempfile::TempDir;

#[test]
fn our_pack_order_matches_git_midx_reverse_index() {
	let repo = TempDir::new().expect("tempdir");
	let dir = repo.path();
	let git = |args: &[&str]| run_git(dir, args);

	// Pin SHA-1 where git supports it (the flag arrived with SHA-256; older git is sha1-only).
	if !try_git(dir, &["init", "-q", "--object-format=sha1", "."]) {
		run_git(dir, &["init", "-q", "."]);
	}
	git(&["config", "user.email", "t@example.com"]);
	git(&["config", "user.name", "Tester"]);
	git(&["config", "commit.gpgSign", "false"]);

	// Three packs kept separate: each round commits, repacks loose into a new pack, and `.keep`s the
	// existing packs so the next repack leaves them alone.
	for round in 1..=3 {
		for n in 1..=2 {
			std::fs::write(
				dir.join(format!("f{round}_{n}.txt")),
				format!("r{round}n{n}\n"),
			)
			.expect("write file");
			git(&["add", "-A"]);
			git(&["commit", "-q", "-m", &format!("r{round}c{n}")]);
		}
		git(&["repack", "-q", "-d"]);
		for keep in keep_targets(&dir.join(".git/objects/pack")) {
			std::fs::write(&keep, b"").expect("write .keep");
		}
	}
	// Drop the .keep marks so the MIDX (and its bitmap) covers every pack.
	for keep in keep_targets(&dir.join(".git/objects/pack")) {
		std::fs::remove_file(&keep).expect("remove .keep");
	}

	if !try_git(dir, &["multi-pack-index", "write", "--bitmap"]) {
		eprintln!("skipping: git without MIDX bitmap support");
		return;
	}
	let bytes = std::fs::read(dir.join(".git/objects/pack/multi-pack-index")).expect("read MIDX");
	let midx = decode_multi_pack_index::<Sha1>(&bytes).expect("decode git's MIDX");

	let Some(git_order) = midx.reverse_index() else {
		eprintln!("skipping: git wrote no RIDX chunk");
		return;
	};
	assert_eq!(git_order.len(), midx.len(), "RIDX covers every object");

	// Reconstruct each object's (pack_id, offset) in lexical order, then reproduce git's order. The
	// preferred pack is whichever owns bitmap position 0.
	let locations: Vec<(u32, u64)> = midx
		.object_ids()
		.iter()
		.map(|id| {
			let (pack, offset) = midx.lookup(id).expect("id present");
			(pack as u32, offset)
		})
		.collect();
	let preferred = locations[git_order[0] as usize].0;
	assert_eq!(
		pack_order(&locations, preferred),
		git_order,
		"our bitmap object order matches git's RIDX (preferred pack {preferred})",
	);

	// The preferred pack really does lead the order.
	assert!(
		git_order
			.iter()
			.take_while(|&&lex| locations[lex as usize].0 == preferred)
			.count()
			> 0,
		"preferred pack leads the bitmap order",
	);
}

/// The `.keep` path for every `.pack` currently in `pack_dir`.
fn keep_targets(pack_dir: &Path) -> Vec<std::path::PathBuf> {
	std::fs::read_dir(pack_dir)
		.expect("read pack dir")
		.filter_map(|e| e.ok().map(|e| e.path()))
		.filter(|p| p.extension().and_then(|x| x.to_str()) == Some("pack"))
		.map(|p| p.with_extension("keep"))
		.collect()
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

/// Run git, returning whether it succeeded (for optional flags/features older git may lack).
fn try_git(dir: &Path, args: &[&str]) -> bool {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}
