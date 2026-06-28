use std::path::PathBuf;
use std::process::Command;

use gitana_worktree::Index;

#[test]
fn round_trips_a_real_git_index() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("index");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("README.md"), b"readme\n").unwrap();
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/lib.rs"), b"lib\n").unwrap();
	std::fs::write(work.join("src/main.rs"), b"main\n").unwrap();
	// Have git write a version-4 index so we exercise v4 parsing.
	git(&["-C", w, "-c", "index.version=4", "add", "."]);

	let git_listing = ls_files(w);

	// Our parser reads git's v4 index and agrees on mode/oid/path.
	let index = Index::parse(&std::fs::read(work.join(".git/index")).unwrap()).expect("parse");
	let ours: Vec<String> = index
		.entries
		.iter()
		.map(|e| format!("{:o} {} {}\t{}", e.mode, e.oid.to_hex(), e.stage, e.path))
		.collect();
	assert_eq!(ours, git_listing, "our parse must match git ls-files");

	// git reads our re-written v4 index identically.
	std::fs::write(work.join(".git/index"), index.write_v4()).unwrap();
	assert_eq!(ls_files(w), git_listing, "git must read our v4 index");
	// And git is otherwise happy with the index we wrote.
	git(&["-C", w, "status", "--porcelain"]);

	std::fs::remove_dir_all(&work).ok();
}

fn ls_files(work: &str) -> Vec<String> {
	git(&["-C", work, "ls-files", "--stage"])
		.lines()
		.map(str::to_owned)
		.collect()
}

fn git(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn unique_tmp(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("gitana-worktree-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	let probe = unique_tmp("probe");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
