#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;

use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{WorkTree, WorktreeError};

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::new(&git_dir)));
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
		.commit_tree(ObjectId::<Sha256>::from_hex(&first).unwrap())
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
		.commit_tree(ObjectId::<Sha256>::from_hex(&first).unwrap())
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

#[tokio::test]
async fn checkout_switches_file_directory_type_without_force() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-typechange");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Tree A: `thing` is a file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Tree B: `thing` is a directory.
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");
	let b = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

	let wt = make_repo(&work);
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&a).unwrap())
		.await
		.unwrap();
	let tree_b = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&b).unwrap())
		.await
		.unwrap();

	// At B (directory). Without force and with a clean tree, checking out A replaces the
	// directory with the file — git does this without `-f`.
	wt.checkout(tree_a, false).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"FILE\n");
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "thing");

	// Back to B replaces the file with the directory.
	wt.checkout(tree_b, false).await.unwrap();
	assert!(work.join("thing").is_dir());
	assert_eq!(
		std::fs::read(work.join("thing/child.txt")).unwrap(),
		b"CHILD\n"
	);
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "thing/child.txt");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_to_delete_untracked_file_in_replaced_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-untracked-dir");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");

	// An untracked file inside the directory that a file would replace.
	std::fs::write(work.join("thing/extra.txt"), b"EXTRA\n").unwrap();
	let wt = make_repo(&work);
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&a).unwrap())
		.await
		.unwrap();

	// Without force, refuse and destroy nothing.
	assert!(matches!(
		wt.checkout(tree_a, false).await,
		Err(WorktreeError::UntrackedOverwrite(_))
	));
	assert_eq!(
		std::fs::read(work.join("thing/extra.txt")).unwrap(),
		b"EXTRA\n"
	);
	assert_eq!(
		std::fs::read(work.join("thing/child.txt")).unwrap(),
		b"CHILD\n"
	);
	// With force it proceeds.
	wt.checkout(tree_a, true).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_untracked_file_at_target_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-untracked-target");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("base.txt"), b"base\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "C0");
	std::fs::write(work.join("new.txt"), b"TRACKED\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "C1");
	let c1 = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Back to C0 (new.txt untracked), then leave an untracked new.txt in the way.
	git(&["-C", w, "reset", "--hard", "HEAD~1"]);
	std::fs::write(work.join("new.txt"), b"UNTRACKED\n").unwrap();

	let wt = make_repo(&work);
	let tree_c1 = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&c1).unwrap())
		.await
		.unwrap();
	assert!(matches!(
		wt.checkout(tree_c1, false).await,
		Err(WorktreeError::UntrackedOverwrite(_))
	));
	assert_eq!(std::fs::read(work.join("new.txt")).unwrap(), b"UNTRACKED\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_overwrites_ignored_untracked_file_at_target_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-ignored-target");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.log\n").unwrap();
	std::fs::write(work.join("base.txt"), b"base\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "C0");
	std::fs::write(work.join("ignored.log"), b"TRACKED\n").unwrap();
	git(&["-C", w, "add", "-f", "ignored.log"]);
	commit(w, "C1");
	let c1 = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Back to C0 (ignored.log untracked), then an ignored untracked ignored.log in the way.
	git(&["-C", w, "reset", "--hard", "HEAD~1"]);
	std::fs::write(work.join("ignored.log"), b"LOCAL\n").unwrap();

	let wt = make_repo(&work);
	let tree_c1 = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&c1).unwrap())
		.await
		.unwrap();
	// The obstruction is ignored, so checkout proceeds and overwrites it, matching git.
	wt.checkout(tree_c1, false).await.unwrap();
	assert_eq!(
		std::fs::read(work.join("ignored.log")).unwrap(),
		b"TRACKED\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_replaces_wholly_ignored_directory_with_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-ignored-wholedir");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"thing/\n").unwrap();
	std::fs::write(work.join("base.txt"), b"base\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A0");
	// `thing` as a tracked file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "thing"]);
	commit(w, "Afile");
	let afile = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Back to A0, then `thing` as a (force-added) directory.
	git(&["-C", w, "reset", "--hard", "HEAD~1"]);
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-f", "thing/child.txt"]);
	commit(w, "Bdir");
	// An untracked file under the ignored directory.
	std::fs::write(work.join("thing/extra.txt"), b"EXTRA\n").unwrap();

	let wt = make_repo(&work);
	let tree_file = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&afile).unwrap())
		.await
		.unwrap();
	// The directory `thing/` is wholly ignored, so it's expendable: checkout proceeds, like git.
	wt.checkout(tree_file, false).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"FILE\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_overwrites_ignored_file_in_replaced_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-ignored-dir");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.log\n").unwrap();
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");

	// Only an ignored untracked file lives in the directory: expendable, so checkout proceeds
	// (and overwrites it) just like git.
	std::fs::write(work.join("thing/skip.log"), b"LOG\n").unwrap();
	let wt = make_repo(&work);
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&a).unwrap())
		.await
		.unwrap();
	wt.checkout(tree_a, false).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"FILE\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_to_delete_untracked_ancestor_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-untracked-anc");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("base.txt"), b"base\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "C0");
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "C1");
	let c1 = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Back to C0 (so `thing` is not tracked), then leave an untracked file `thing` in the way.
	git(&["-C", w, "reset", "--hard", "HEAD~1"]);
	std::fs::write(work.join("thing"), b"UNTRACKED\n").unwrap();

	let wt = make_repo(&work);
	let tree_c1 = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&c1).unwrap())
		.await
		.unwrap();
	assert!(matches!(
		wt.checkout(tree_c1, false).await,
		Err(WorktreeError::UntrackedOverwrite(_))
	));
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"UNTRACKED\n");

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
	// A per-call sequence number keeps every temp dir distinct even for the same tag, so
	// tests running in parallel threads never race on a shared path.
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!(
		"gitana-worktree-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	// Probe once per test binary: every test calls this, and a shared probe dir raced under
	// load. `OnceLock` makes it concurrency-safe and spawns `git init` a single time.
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-checkout");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
