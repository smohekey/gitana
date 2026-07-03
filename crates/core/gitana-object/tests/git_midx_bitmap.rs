//! Differential test: [`gitana_object::decode_midx_bitmap`] reads the reachability `.bitmap` stock
//! `git multi-pack-index write --bitmap` produces, and the reachable object set it reports for each
//! bitmapped commit matches `git rev-list --objects <commit>` exactly.
//!
//! Uses a SHA-1 repo (git's default); skips when git is too old for MIDX bitmaps (< 2.34).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use gitana_object::{Sha1, decode_midx_bitmap, decode_multi_pack_index};
use tempfile::TempDir;

#[test]
fn our_reachability_matches_git_rev_list() {
	let repo = TempDir::new().expect("tempdir");
	let dir = repo.path();

	if !try_git(dir, &["init", "-q", "--object-format=sha1", "."]) {
		run_git(dir, &["init", "-q", "."]);
	}
	run_git(dir, &["config", "user.email", "t@example.com"]);
	run_git(dir, &["config", "user.name", "Tester"]);
	run_git(dir, &["config", "commit.gpgSign", "false"]);

	// A branchy history so several commits get bitmapped and reachability sets differ.
	for n in 1..=4 {
		std::fs::write(dir.join(format!("f{n}.txt")), format!("a{n}\n")).expect("write");
		std::fs::write(dir.join("shared.txt"), format!("s{n}\n")).expect("write shared");
		run_git(dir, &["add", "-A"]);
		run_git(dir, &["commit", "-q", "-m", &format!("c{n}")]);
	}
	// Capture the initial branch rather than assuming "main" (it may be "master" or configured).
	let default_branch = git_stdout(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
	run_git(dir, &["checkout", "-q", "-b", "side", "HEAD~2"]);
	for n in 5..=6 {
		std::fs::write(dir.join(format!("g{n}.txt")), format!("b{n}\n")).expect("write");
		run_git(dir, &["add", "-A"]);
		run_git(dir, &["commit", "-q", "-m", &format!("s{n}")]);
	}
	run_git(dir, &["checkout", "-q", &default_branch]);
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

	let bitmap_bytes = std::fs::read(find_bitmap(&pack_dir)).expect("read .bitmap");
	let index = decode_midx_bitmap::<Sha1>(&bitmap_bytes).expect("decode bitmap");

	// The bitmap is bound to this MIDX (its trailing checksum).
	assert_eq!(
		index.midx_checksum(),
		&midx_bytes[midx_bytes.len() - 20..],
		"bitmap names the MIDX checksum",
	);

	// Every bitmapped commit's reachable object set matches `git rev-list --objects`. An entry's
	// position is a lexical (OIDL) index, so the commit id is `object_ids()[position]`.
	let mut checked = 0;
	for position in index.bitmapped_commit_positions() {
		let commit = midx.object_ids()[position as usize];
		let ours: HashSet<String> = index
			.reachable_from(&commit, &midx)
			.expect("reachable set")
			.iter()
			.map(|id| id.to_string())
			.collect();
		let git = rev_list_objects(dir, &commit.to_string());
		assert_eq!(
			ours, git,
			"reachability for commit {commit} matches git rev-list"
		);
		checked += 1;
	}
	assert!(checked > 0, "at least one commit was bitmapped and checked");
}

/// The object ids `git rev-list --objects <commit>` reports (the first token of each line).
fn rev_list_objects(dir: &Path, commit: &str) -> HashSet<String> {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(["rev-list", "--objects", commit])
		.output()
		.expect("run git rev-list");
	assert!(output.status.success(), "git rev-list failed");
	String::from_utf8(output.stdout)
		.expect("utf8")
		.lines()
		.filter_map(|line| line.split_whitespace().next().map(str::to_owned))
		.collect()
}

fn find_bitmap(pack_dir: &Path) -> std::path::PathBuf {
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

/// Run git and return its trimmed stdout.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git");
	assert!(output.status.success(), "git {args:?} failed");
	String::from_utf8(output.stdout)
		.expect("utf8")
		.trim()
		.to_owned()
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
