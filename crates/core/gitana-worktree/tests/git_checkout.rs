#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;

use gitana_object::ObjectId;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{WorkTree, WorktreeError};

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	WorkTree::new(repo, work, git_dir)
}

/// Build two commits; return (work dir, first commit id).
fn two_commits(tag: &str) -> (PathBuf, String) {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/b.txt"), b"B\n").unwrap();
	let run = work.join("run.sh");
	std::fs::write(&run, b"#!/bin/sh\n").unwrap();
	std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let first = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	std::fs::write(work.join("c.txt"), b"C\n").unwrap();
	std::fs::remove_file(work.join("sub/b.txt")).unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "two");

	(work, first)
}

#[tokio::test]
async fn checkout_materialises_a_tree_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let (work, first) = two_commits("checkout");
	let w = work.to_str().unwrap();

	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&first).unwrap())
		.await
		.unwrap();
	wt.checkout(tree1, true).await.unwrap();

	// Worktree restored to the first commit.
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert_eq!(std::fs::read(work.join("sub/b.txt")).unwrap(), b"B\n");
	assert!(!work.join("c.txt").exists());
	assert!(
		std::fs::metadata(work.join("run.sh"))
			.unwrap()
			.permissions()
			.mode()
			& 0o111
			!= 0
	);

	// git agrees the working tree and index both equal the first tree.
	assert!(
		git(&["-C", w, "diff", &first]).is_empty(),
		"worktree must equal tree"
	);
	assert!(
		git(&["-C", w, "diff", "--cached", &first]).is_empty(),
		"index must equal tree"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_to_clobber_dirty_files() {
	if !git_supports_sha256() {
		return;
	}
	let (work, first) = two_commits("checkout-conflict");
	// a.txt currently A2 (committed); make it dirty.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();

	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&first).unwrap())
		.await
		.unwrap();

	// Without force, the dirty a.txt (which tree1 would change) blocks checkout.
	assert!(matches!(
		wt.checkout(tree1, false).await,
		Err(WorktreeError::Conflict(_))
	));
	// With force it proceeds.
	wt.checkout(tree1, true).await.unwrap();
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");

	std::fs::remove_dir_all(&work).ok();
}

fn commit(work: &str, msg: &str) {
	git(&[
		"-C",
		work,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		msg,
	]);
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
	let probe = unique_tmp("probe-checkout");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
