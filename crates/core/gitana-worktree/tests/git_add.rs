#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

#[tokio::test]
async fn add_stages_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("add");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("README.md"), b"readme\n").unwrap();
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/lib.rs"), b"lib\n").unwrap();
	let script = work.join("run.sh");
	std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
	std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
	std::os::unix::fs::symlink("README.md", work.join("link")).unwrap();

	let paths = ["README.md", "src/lib.rs", "run.sh", "link"];

	// Stage with our worktree.
	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	WorkTree::new(repo, &work, &git_dir)
		.add(&paths)
		.await
		.unwrap();
	let ours = ls_files(w);

	// Stage the same paths with git from an empty index.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	let mut args = vec!["-C", w, "add"];
	args.extend_from_slice(&paths);
	git(&args);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs, "our add must stage identically to git add");
	// Sanity: exec bit and symlink modes are present.
	assert!(
		ours
			.iter()
			.any(|l| l.starts_with("100755 ") && l.ends_with("\trun.sh"))
	);
	assert!(
		ours
			.iter()
			.any(|l| l.starts_with("120000 ") && l.ends_with("\tlink"))
	);

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
	let probe = unique_tmp("probe-add");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
