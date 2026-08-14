#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};

use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{IndexEntry, Stat, WorkTree, WorktreeError};

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore, CapWorkDir, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
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
	wt.checkout(tree1, true, None).await.unwrap();

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
		wt.checkout(tree1, false, None).await,
		Err(WorktreeError::Conflict(_))
	));
	// With force it proceeds.
	wt.checkout(tree1, true, None).await.unwrap();
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
	wt.checkout(tree_a, false, None).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"FILE\n");
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "thing");

	// Back to B replaces the file with the directory.
	wt.checkout(tree_b, false, None).await.unwrap();
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
		wt.checkout(tree_a, false, None).await,
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
	wt.checkout(tree_a, true, None).await.unwrap();
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
		wt.checkout(tree_c1, false, None).await,
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
	wt.checkout(tree_c1, false, None).await.unwrap();
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
	wt.checkout(tree_file, false, None).await.unwrap();
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
	wt.checkout(tree_a, false, None).await.unwrap();
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
		wt.checkout(tree_c1, false, None).await,
		Err(WorktreeError::UntrackedOverwrite(_))
	));
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"UNTRACKED\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_merge_applies_file_to_dir_type_change() {
	// A two-tree merge that turns a tracked file `thing` into a directory `thing/child` must remove the old
	// file before writing the child — a write-first order leaves the working tree half-changed and then
	// fails to remove the now-directory. The whole diff applies (nothing would be overwritten).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("twoway-typechange");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "from");
	let from_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "rm", "-q", "thing"]);
	std::fs::create_dir_all(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "to");
	let to_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();

	let wt = make_repo(&work);
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
		true,
		None,
	)
	.await
	.unwrap();
	wt.checkout_merge(
		ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
		ObjectId::<Sha256>::from_hex(&to_tree).unwrap(),
		None,
	)
	.await
	.expect("a file→directory type-change two-tree merge must apply");
	assert_eq!(
		std::fs::read(work.join("thing/child")).unwrap(),
		b"CHILD\n",
		"the child file must be written"
	);
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"keep\nthing/child",
		"the index must reflect the type change"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_merge_applies_case_rename() {
	// A two-tree merge that renames `Foo`→`foo` (case only) under `core.ignoreCase` must APPLY, not refuse:
	// git's index is case-insensitive, so the to-side `foo` is the same tracked path, not an untracked
	// obstruction. Result: the merge applies (nothing would be overwritten) and the index is the to-side
	// case (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("twoway-case-rename");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "from");
	let from_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "rm", "-q", "Foo"]);
	std::fs::write(work.join("foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "foo"]);
	commit(w, "to");
	let to_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();

	let wt = make_repo(&work);
	// Put the working state at `from` (Foo), then two-tree-merge from→to (the case-rename fast-forward).
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
		true,
		None,
	)
	.await
	.unwrap();
	wt.checkout_merge(
		ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
		ObjectId::<Sha256>::from_hex(&to_tree).unwrap(),
		None,
	)
	.await
	.expect("a clean case-rename two-tree merge must apply, not refuse");
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"foo",
		"the index must be the to-side case (foo)"
	);
	let content = std::fs::read(work.join("foo"))
		.or_else(|_| std::fs::read(work.join("Foo")))
		.unwrap();
	assert_eq!(
		content, b"content\n",
		"the file content must survive the rename"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_merge_case_colliding_removal_refuses_deterministically() {
	// The two-tree-merge analogue of `checkout_case_colliding_removal_refuses_deterministically`: a colliding
	// staged index (`Foo`=AAA, `foo`=BBB) merged to a to-tree that drops the whole fold-key must
	// refuse when the single shared working file is dirty relative to a colliding entry — not silently remove
	// it. A prior version tested cleanliness against an arbitrarily-kept folded entry, so the merge could pass
	// and delete the file depending on `HashMap` ordering; this pins a deterministic refuse across repeats.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	if !case_insensitive_fs() {
		eprintln!(
			"skipping: case-sensitive filesystem (the case-colliding scenario needs a shared inode)"
		);
		return;
	}
	let work = unique_tmp("twoway-case-collide-removal");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// A `z`-only tree is both the merge base and the to-side (it drops `Foo`/`foo`).
	std::fs::write(work.join("z"), b"z\n").unwrap();
	git(&["-C", w, "add", "z"]);
	commit(w, "base");
	let to_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	// Two distinct blobs, then a colliding staged index `{z, Foo=AAA, foo=BBB}` captured as the from-tree.
	std::fs::write(work.join("blobsrc"), b"AAA\n").unwrap();
	let blob_a = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::write(work.join("blobsrc"), b"BBB\n").unwrap();
	let blob_b = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::remove_file(work.join("blobsrc")).unwrap();
	for spec in [
		format!("100644,{blob_a},Foo"),
		format!("100644,{blob_b},foo"),
	] {
		git(&[
			"-C",
			w,
			"-c",
			"core.ignoreCase=false",
			"update-index",
			"--add",
			"--cacheinfo",
			&spec,
		]);
	}
	let from_tree = git(&["-C", w, "write-tree"]).trim().to_owned();
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	// The shared working file holds `Foo`'s blob — clean vs `Foo`, dirty vs `foo`.
	std::fs::write(work.join("Foo"), b"AAA\n").unwrap();

	let wt = make_repo(&work);
	for attempt in 0..8 {
		let result = wt
			.checkout_merge(
				ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
				ObjectId::<Sha256>::from_hex(&to_tree).unwrap(),
				None,
			)
			.await;
		assert!(
			result.is_err(),
			"attempt {attempt}: a colliding merge removal dirty vs a colliding entry must refuse"
		);
		let content = std::fs::read(work.join("Foo"))
			.or_else(|_| std::fs::read(work.join("foo")))
			.unwrap();
		assert_eq!(
			content, b"AAA\n",
			"attempt {attempt}: the shared working file must survive the refused fast-forward"
		);
	}
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_preserves_staged_case_rename() {
	// Under `core.ignoreCase`, a switch that KEEPS `Foo` must not overwrite a locally STAGED `Foo`->`foo`
	// rename: git carries the staged `foo` forward (probed vs git 2.55: `D Foo` / `A foo`), it does not
	// rewrite it back to `Foo`. The three-way (HEAD-aware) checkout distinguishes this staged recase (index
	// diverges from HEAD) from a genuine branch rename (index matches HEAD, which is applied).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-preserve-staged-recase");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let tree_foo = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	// Stage a rename `Foo`->`foo` (index becomes `foo`; HEAD stays `Foo`).
	git(&["-C", w, "mv", "Foo", "foo"]);

	let wt = make_repo(&work);
	// Switch to a tree that keeps `Foo`; the staged `foo` must survive (index and file stay `foo`).
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_foo).unwrap(),
		false,
		None,
	)
	.await
	.expect("a switch keeping Foo over a staged Foo->foo rename must succeed, as git does");
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"foo",
		"a staged case-rename must be preserved, not rewritten to the old case"
	);
	let content = std::fs::read(work.join("foo"))
		.or_else(|_| std::fs::read(work.join("Foo")))
		.unwrap();
	assert_eq!(
		content, b"content\n",
		"the staged file's content must survive"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_merge_refuses_staged_recase_on_modify() {
	// A two-tree merge that MODIFIES `Foo` while the index holds a staged `Foo`->`foo` rename must refuse: the
	// incoming write would overwrite the staged rename (probed vs git 2.55: "local changes to Foo would be
	// overwritten by merge", aborts). The fold fallback must not mask the staged recase as clean.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("twoway-staged-recase-modify");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"X\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();
	let from_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", "-c", "up"]);
	std::fs::write(work.join("Foo"), b"Z\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "modify Foo");
	let to_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", &base_branch]);
	// Stage the rename `Foo`->`foo` on top of `from`.
	git(&["-C", w, "mv", "Foo", "foo"]);

	let wt = make_repo(&work);
	let result = wt
		.checkout_merge(
			ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
			ObjectId::<Sha256>::from_hex(&to_tree).unwrap(),
			None,
		)
		.await;
	assert!(
		result.is_err(),
		"a two-tree merge modifying a staged-renamed file must refuse, as git does"
	);
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"foo",
		"the refused merge must leave the staged rename intact"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_merge_preserves_staged_recase_on_delete() {
	// A two-tree merge that DELETES `Foo` while the index holds a staged `Foo`->`foo` rename must proceed and
	// keep the staged `foo` (probed vs git 2.55: the delete fast-forwards, `foo` survives). gta must not
	// remove the shared inode when applying the delete.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("twoway-staged-recase-delete");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"X\n").unwrap();
	std::fs::write(work.join("keep"), b"K\n").unwrap();
	git(&["-C", w, "add", "Foo", "keep"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();
	let from_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", "-c", "up"]);
	git(&["-C", w, "rm", "-q", "Foo"]);
	commit(w, "delete Foo");
	let to_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", &base_branch]);
	git(&["-C", w, "mv", "Foo", "foo"]);

	let wt = make_repo(&work);
	wt.checkout_merge(
		ObjectId::<Sha256>::from_hex(&from_tree).unwrap(),
		ObjectId::<Sha256>::from_hex(&to_tree).unwrap(),
		None,
	)
	.await
	.expect("a two-tree merge deleting a staged-renamed file must proceed, as git does");
	let entries = git(&["-C", w, "ls-files"]);
	assert!(
		entries.contains("foo") && entries.contains("keep") && !entries.contains("Foo"),
		"the staged rename `foo` must be preserved across the delete fast-forward: {entries}"
	);
	assert!(
		work.join("foo").exists() || work.join("Foo").exists(),
		"the staged file must not be deleted with the removed `Foo`"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_target_modify_over_staged_case_rename() {
	// Under `core.ignoreCase`, a staged `Foo`->`foo` rename plus a destination that MODIFIES `Foo` must
	// refuse: the incoming edit conflicts with the staged rename (probed vs git 2.55: "local changes to Foo
	// would be overwritten"). Only a destination that keeps `Foo` UNCHANGED from HEAD may preserve the recase.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-recase-vs-modify");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"X\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", "-c", "up"]);
	std::fs::write(work.join("Foo"), b"Z\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "modify Foo");
	let tree_up = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", &base_branch]);
	// Stage the rename `Foo`->`foo` (index=foo; HEAD keeps Foo).
	git(&["-C", w, "mv", "Foo", "foo"]);

	let wt = make_repo(&work);
	assert!(
		wt.checkout(ObjectId::<Sha256>::from_hex(&tree_up).unwrap(), false, None,)
			.await
			.is_err(),
		"a destination that modifies a staged-renamed file must refuse, as git does"
	);
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"foo",
		"the refused checkout must leave the staged rename intact"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_colliding_third_spelling_refuses() {
	// A case-colliding index (`Foo`=AAA, `foo`=BBB) whose target RECASES the key to a THIRD spelling (`FOO`):
	// git checks the shared working file against EVERY colliding entry and refuses if dirty vs any (probed vs
	// git 2.55: "local changes to foo would be overwritten"). The guard must check all colliding entries, not
	// one arbitrarily-kept blob.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	if !case_insensitive_fs() {
		eprintln!(
			"skipping: case-sensitive filesystem (the case-colliding scenario needs a shared inode)"
		);
		return;
	}
	let work = unique_tmp("checkout-collide-third");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("z"), b"z\n").unwrap();
	git(&["-C", w, "add", "z"]);
	commit(w, "base");
	std::fs::write(work.join("blobsrc"), b"AAA\n").unwrap();
	let blob_a = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::write(work.join("blobsrc"), b"BBB\n").unwrap();
	let blob_b = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::write(work.join("blobsrc"), b"Z\n").unwrap();
	let blob_z = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::remove_file(work.join("blobsrc")).unwrap();
	// Commit the colliding pair so HEAD tracks both (the fold-aware guard's `all-colliding` check, not the
	// staged-recase refuse, is what must fire here).
	for spec in [
		format!("100644,{blob_a},Foo"),
		format!("100644,{blob_b},foo"),
	] {
		git(&[
			"-C",
			w,
			"-c",
			"core.ignoreCase=false",
			"update-index",
			"--add",
			"--cacheinfo",
			&spec,
		]);
	}
	commit(w, "colliding");
	// Build a target tree that drops `Foo`/`foo` for a third spelling `FOO`=Z, then restore the index.
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"rm",
		"-q",
		"--cached",
		"Foo",
		"foo",
	]);
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("100644,{blob_z},FOO"),
	]);
	let tree_target = git(&["-C", w, "write-tree"]).trim().to_owned();
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"rm",
		"-q",
		"--cached",
		"FOO",
	]);
	for spec in [
		format!("100644,{blob_a},Foo"),
		format!("100644,{blob_b},foo"),
	] {
		git(&[
			"-C",
			w,
			"-c",
			"core.ignoreCase=false",
			"update-index",
			"--add",
			"--cacheinfo",
			&spec,
		]);
	}
	// The shared working file holds `Foo`'s blob — clean vs `Foo`, dirty vs `foo`.
	std::fs::write(work.join("Foo"), b"AAA\n").unwrap();

	let wt = make_repo(&work);
	assert!(
		wt.checkout(
			ObjectId::<Sha256>::from_hex(&tree_target).unwrap(),
			false,
			None,
		)
		.await
		.is_err(),
		"a recase to a third spelling over a colliding index dirty vs an entry must refuse, as git does"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_preflights_unrelated_blob_before_removing_rename_source() {
	// A checkout combining a case-rename (`Foo`->`foo`) with an UNRELATED target path whose blob is missing
	// must abort BEFORE removing the rename source, so neither casing is lost. The removal-before-write phase
	// (a case-rename recases in place) makes this a real data-loss risk if only the rename's own blob is
	// preflighted; every materialised blob must be validated up front, as git validates objects before mutating.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-preflight-unrelated");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"shared\n").unwrap();
	std::fs::write(work.join("aaa"), b"A\n").unwrap();
	git(&["-C", w, "add", "Foo", "aaa"]);
	commit(w, "upper");
	let tree_upper = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let upper = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// lower: rename Foo->foo (blob unchanged) and modify aaa->B (a distinct blob we will corrupt).
	git(&["-C", w, "rm", "-q", "Foo"]);
	std::fs::write(work.join("foo"), b"shared\n").unwrap();
	std::fs::write(work.join("aaa"), b"B\n").unwrap();
	git(&["-C", w, "add", "foo", "aaa"]);
	commit(w, "lower");
	let tree_lower = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let bad = git(&["-C", w, "rev-parse", "HEAD:aaa"]).trim().to_owned();
	// Genuine rename context: HEAD at `upper` so the index `Foo` matches HEAD.
	git(&["-C", w, "update-ref", "--no-deref", "HEAD", &upper]);

	let wt = make_repo(&work);
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_upper).unwrap(),
		true,
		None,
	)
	.await
	.unwrap();
	// Corrupt the UNRELATED `aaa` blob (not the rename target `foo`).
	std::fs::remove_file(work.join(".git/objects").join(&bad[..2]).join(&bad[2..])).unwrap();

	assert!(
		wt.checkout(
			ObjectId::<Sha256>::from_hex(&tree_lower).unwrap(),
			false,
			None
		)
		.await
		.is_err(),
		"a missing unrelated blob must abort the checkout"
	);
	let content = std::fs::read(work.join("Foo"))
		.or_else(|_| std::fs::read(work.join("foo")))
		.unwrap();
	assert_eq!(
		content, b"shared\n",
		"the case-rename source must survive an abort on an unrelated missing blob"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_colliding_addition_recase_refuses() {
	// HEAD and the index both keep `Foo`; the index ADDITIONALLY stages `foo` with a distinct blob (a
	// colliding addition, not a rename). Switching to a branch that recases `Foo`->`FOO` must refuse — the
	// shared working file is dirty relative to the staged `foo` (probed vs git 2.55: aborts). The recase must
	// not be silently skipped by misclassifying `foo` as a preservable staged rename.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	if !case_insensitive_fs() {
		eprintln!(
			"skipping: case-sensitive filesystem (the case-colliding scenario needs a shared inode)"
		);
		return;
	}
	let work = unique_tmp("checkout-collide-add-recase");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"X\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();
	let blob_x = git(&["-C", w, "rev-parse", "HEAD:Foo"]).trim().to_owned();
	// Target branch `up` recases Foo->FOO with unchanged content.
	git(&["-C", w, "switch", "-q", "-c", "up"]);
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"rm",
		"-q",
		"--cached",
		"Foo",
	]);
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("100644,{blob_x},FOO"),
	]);
	commit(w, "recase FOO");
	let tree_up = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "switch", "-q", &base_branch]);
	// Stage a colliding `foo` with a DISTINCT blob alongside the retained `Foo`.
	std::fs::write(work.join("blobsrc"), b"Y\n").unwrap();
	let blob_y = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::remove_file(work.join("blobsrc")).unwrap();
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("100644,{blob_y},foo"),
	]);
	std::fs::write(work.join("Foo"), b"X\n").unwrap();

	let wt = make_repo(&work);
	assert!(
		wt.checkout(ObjectId::<Sha256>::from_hex(&tree_up).unwrap(), false, None,)
			.await
			.is_err(),
		"a recase over a dirty colliding addition must refuse, as git does"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn force_checkout_validates_exclude_files() {
	// `--force` skips the overwrite *protection* but not config validation: a directory
	// `.git/info/exclude` is fatal to git even under `checkout -f`/`switch -f` (probed vs git 2.55). So a
	// forced checkout must still error on it, not silently skip the read.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let (work, first) = two_commits("checkout-force-validate");
	let git_dir = work.join(".git");
	std::fs::remove_file(git_dir.join("info").join("exclude")).ok();
	std::fs::create_dir_all(git_dir.join("info").join("exclude")).unwrap();

	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&first).unwrap())
		.await
		.unwrap();
	assert!(
		matches!(
			wt.checkout(tree1, true, None).await,
			Err(WorktreeError::ExcludeFile(_))
		),
		"a forced checkout must still validate a directory .git/info/exclude, like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_colliding_index_preserves_file() {
	// A case-colliding index (`Foo` and `foo` both staged, e.g. from a case-sensitive-FS commit) under
	// core.ignoreCase=true: switching to a target that keeps only `Foo` must NOT delete the shared working
	// file — on a case-insensitive filesystem `foo` is the same inode as the retained `Foo`. git refuses
	// such a switch; we preserve the file (dropping only the stale colliding index entry).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-case-collide");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("Foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let tree_foo = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let blob = git(&["-C", w, "rev-parse", "HEAD:Foo"]).trim().to_owned();
	// Force a colliding lowercase `foo` index entry alongside `Foo` (case-sensitive so git adds both).
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("100644,{blob},foo"),
	]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	// A local edit to the shared working file.
	std::fs::write(work.join("Foo"), b"LOCAL EDIT\n").unwrap();

	let wt = make_repo(&work);
	// Switch to a tree that keeps only `Foo`; whatever the result, the shared file must survive.
	let _ = wt
		.checkout(
			ObjectId::<Sha256>::from_hex(&tree_foo).unwrap(),
			false,
			None,
		)
		.await;
	let content = std::fs::read(work.join("Foo"))
		.or_else(|_| std::fs::read(work.join("foo")))
		.unwrap();
	assert_eq!(
		content, b"LOCAL EDIT\n",
		"the shared working file of a case-colliding index must not be deleted"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_colliding_retained_preserves_staged_entry() {
	// A case-colliding index (`Foo`=AAA committed, `foo`=BBB a distinct staged blob) under
	// core.ignoreCase=true, switched to a target that RETAINS `Foo`: git preserves the colliding staged `foo`
	// entry — index keeps BOTH spellings, status `AM foo` (probed vs git 2.55). The fold-aware checkout must
	// too: it may not silently drop `foo`'s distinct staged content. (An earlier version dropped the stale
	// index entry, discarding that blob.)
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-case-collide-retain");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("Foo"), b"AAA\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "base");
	let tree_foo = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	// Stage a colliding lowercase `foo` with a DISTINCT blob (added case-sensitively).
	std::fs::write(work.join("blobsrc"), b"BBB\n").unwrap();
	let blob_b = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::remove_file(work.join("blobsrc")).unwrap();
	git(&[
		"-C",
		w,
		"-c",
		"core.ignoreCase=false",
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("100644,{blob_b},foo"),
	]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	// The shared working file is clean vs the retained `Foo`.
	std::fs::write(work.join("Foo"), b"AAA\n").unwrap();

	let wt = make_repo(&work);
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_foo).unwrap(),
		false,
		None,
	)
	.await
	.expect("a retaining switch over a colliding staged entry must succeed, as git does");
	// The index must still carry BOTH `Foo` and the distinct-blob `foo` — git preserves the staged entry.
	let entries = git(&["-C", w, "ls-files", "-s"]);
	assert!(
		entries.contains("\tFoo") && entries.contains(&format!("{blob_b} 0\tfoo")),
		"the colliding staged `foo` (distinct blob) must be preserved, not dropped: {entries}"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_colliding_removal_refuses_deterministically() {
	// A case-colliding index (`Foo`=AAA, `foo`=BBB, different blobs) under core.ignoreCase=true switched to a
	// target that drops the WHOLE fold-key: git checks the single shared working file against EACH colliding
	// entry's own blob and refuses if it is dirty relative to *any* (probed vs git 2.55 — working=AAA refuses
	// naming `foo`; =BBB names `Foo`; =CCC names both). The fold-aware guard must do the same. A prior version
	// consulted an arbitrarily-kept folded entry, so the verdict flipped between refuse and silent discard
	// under `HashMap` ordering — this pins a deterministic refuse (and the file's survival) across repeats.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	if !case_insensitive_fs() {
		eprintln!(
			"skipping: case-sensitive filesystem (the case-colliding scenario needs a shared inode)"
		);
		return;
	}
	let work = unique_tmp("checkout-case-collide-removal");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// A target tree containing only `z` — neither `Foo` nor `foo`.
	std::fs::write(work.join("z"), b"z\n").unwrap();
	git(&["-C", w, "add", "z"]);
	commit(w, "base");
	let tree_target = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	// Two distinct blobs for the colliding pair.
	std::fs::write(work.join("blobsrc"), b"AAA\n").unwrap();
	let blob_a = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::write(work.join("blobsrc"), b"BBB\n").unwrap();
	let blob_b = git(&["-C", w, "hash-object", "-w", "blobsrc"])
		.trim()
		.to_owned();
	std::fs::remove_file(work.join("blobsrc")).unwrap();
	// A case-colliding index: `Foo`=AAA and `foo`=BBB both at stage 0 (added case-sensitively).
	for spec in [
		format!("100644,{blob_a},Foo"),
		format!("100644,{blob_b},foo"),
	] {
		git(&[
			"-C",
			w,
			"-c",
			"core.ignoreCase=false",
			"update-index",
			"--add",
			"--cacheinfo",
			&spec,
		]);
	}
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	// The single shared working file holds `Foo`'s blob — clean vs `Foo`, dirty vs `foo` — so git refuses.
	std::fs::write(work.join("Foo"), b"AAA\n").unwrap();

	let wt = make_repo(&work);
	// Repeat: a nondeterministic guard would eventually pick the survivor that lets the removal through.
	for attempt in 0..8 {
		let result = wt
			.checkout(
				ObjectId::<Sha256>::from_hex(&tree_target).unwrap(),
				false,
				None,
			)
			.await;
		assert!(
			result.is_err(),
			"attempt {attempt}: a colliding removal dirty vs a colliding entry must refuse, as git does"
		);
		let content = std::fs::read(work.join("Foo"))
			.or_else(|_| std::fs::read(work.join("foo")))
			.unwrap();
		assert_eq!(
			content, b"AAA\n",
			"attempt {attempt}: the shared working file must survive the refused checkout"
		);
	}
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_rename_missing_blob_preserves_file() {
	// If a case-rename's replacement blob is missing (a corrupt object store), checkout must abort BEFORE
	// removing the stale-cased file — its working copy is the only surviving content — rather than delete
	// it and then fail the rewrite.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-caserename-missingblob");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"shared\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "upper");
	let tree_upper = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let upper = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	git(&["-C", w, "rm", "-q", "Foo"]);
	std::fs::write(work.join("foo"), b"shared\n").unwrap();
	git(&["-C", w, "add", "foo"]);
	commit(w, "lower");
	let tree_lower = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let blob = git(&["-C", w, "rev-parse", "HEAD:foo"]).trim().to_owned();
	// Point HEAD at `upper` (the `Foo` commit) so the checkout below is a genuine branch case-rename off it
	// (index `Foo` matches HEAD), not a staged recase — which git, and the fold-aware checkout, would instead
	// preserve. `--no-deref` moves HEAD without touching the working tree.
	git(&["-C", w, "update-ref", "--no-deref", "HEAD", &upper]);

	let wt = make_repo(&work);
	// Put the worktree/index at `Foo`, then corrupt the shared blob before the case-rename checkout.
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_upper).unwrap(),
		true,
		None,
	)
	.await
	.unwrap();
	let obj = work.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
	std::fs::remove_file(&obj).unwrap();

	// A case-rename `Foo`→`foo` whose blob is now missing must abort and preserve the working file.
	assert!(
		wt.checkout(
			ObjectId::<Sha256>::from_hex(&tree_lower).unwrap(),
			false,
			None
		)
		.await
		.is_err(),
		"checkout must abort on the missing replacement blob"
	);
	let content = std::fs::read(work.join("Foo"))
		.or_else(|_| std::fs::read(work.join("foo")))
		.unwrap();
	assert_eq!(
		content, b"shared\n",
		"the stale-cased working file must be preserved when the blob is missing"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_refuses_dirty_case_rename() {
	// git aborts a case-only rename checkout when the working file is locally modified, preserving the
	// edit (probed vs git 2.55: "local changes would be overwritten by checkout"). The fold-aware guard
	// must too — a case-rename is a change even when the blob matches, so a *dirty* one refuses rather
	// than silently rewriting the file under the new case and discarding the edit.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-dirty-case-rename");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"orig\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "upper");
	let tree_upper = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "rm", "-q", "Foo"]);
	std::fs::write(work.join("foo"), b"orig\n").unwrap();
	git(&["-C", w, "add", "foo"]);
	commit(w, "lower");
	// The working file (index/HEAD = `foo`) is now locally modified.
	std::fs::write(work.join("foo"), b"LOCAL EDIT\n").unwrap();

	let wt = make_repo(&work);
	let tree = ObjectId::<Sha256>::from_hex(&tree_upper).unwrap();
	assert!(
		matches!(
			wt.checkout(tree, false, None).await,
			Err(WorktreeError::Conflict(_))
		),
		"a dirty case-rename must refuse, like git"
	);
	let content = std::fs::read(work.join("foo"))
		.or_else(|_| std::fs::read(work.join("Foo")))
		.unwrap();
	assert_eq!(content, b"LOCAL EDIT\n", "the local edit must be preserved");
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_case_rename_preserves_file_like_git() {
	// Under `core.ignoreCase`, git's index is case-insensitive: a `Foo`→`foo` case-rename across a
	// checkout keeps the file (same inode on a case-insensitive filesystem) and rewrites only the index
	// entry to the target case (probed vs git 2.55, both non-force and force). A case-sensitive-only
	// folded ignore match would misread this as an add+delete and remove the re-created file — the P1
	// data-loss this guards against.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-case-rename");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(work.join("Foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "Foo"]);
	commit(w, "upper");
	let tree_upper = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "rm", "-q", "Foo"]);
	std::fs::write(work.join("foo"), b"content\n").unwrap();
	git(&["-C", w, "add", "foo"]);
	commit(w, "lower");
	let tree_lower = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();

	let wt = make_repo(&work);
	// Non-force checkout `foo` (current index) -> `Foo` (target): index must become `Foo`, file kept.
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_upper).unwrap(),
		false,
		None,
	)
	.await
	.unwrap();
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"Foo",
		"a case-rename must leave the index at the target case (Foo)"
	);
	let content = std::fs::read(work.join("Foo"))
		.or_else(|_| std::fs::read(work.join("foo")))
		.unwrap();
	assert_eq!(content, b"content\n", "case-rename must not lose the file");

	// Force checkout `Foo` -> `foo`: same rename via the force path (no guard), still must not delete.
	wt.checkout(
		ObjectId::<Sha256>::from_hex(&tree_lower).unwrap(),
		true,
		None,
	)
	.await
	.unwrap();
	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"foo",
		"a forced case-rename must leave the index at the target case (foo)"
	);
	let content = std::fs::read(work.join("foo"))
		.or_else(|_| std::fs::read(work.join("Foo")))
		.unwrap();
	assert_eq!(
		content, b"content\n",
		"forced case-rename must not lose the file"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_overwrites_ancestor_file_ignored_by_dir_rule() {
	// A checkout that needs `a/foo/bar` when an untracked FILE occupies the ancestor slot `a/foo`, and
	// `.git/info/exclude` contains `a/`: git treats the file as ignored (via the ancestor directory rule)
	// and switches; the overwrite guard's ancestor check must be directory-aware, not leaf-only, so gta
	// does not wrongly report UntrackedOverwrite (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-ancestor-ignored");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("a/foo")).unwrap();
	std::fs::write(work.join("a/foo/bar"), b"bar\n").unwrap();
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "with-foo");
	let with_foo = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "rm", "-q", "-r", "a"]);
	commit(w, "no-a");
	// An untracked FILE at `a/foo` (blocking the `a/foo` directory), ignored via `a/`.
	std::fs::create_dir_all(work.join("a")).unwrap();
	std::fs::write(work.join("a/foo"), b"LOCAL\n").unwrap();
	std::fs::write(git_dir.join("info").join("exclude"), b"a/\n").unwrap();

	let wt = make_repo(&work);
	let tree = ObjectId::<Sha256>::from_hex(&with_foo).unwrap();
	// Must NOT refuse — the in-the-way `a/foo` is ignored (ancestor `a/`), so it is expendable.
	wt.checkout(tree, false, None).await.unwrap();
	assert_eq!(
		std::fs::read(work.join("a/foo/bar")).unwrap(),
		b"bar\n",
		"the ignored ancestor-file must be replaced, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn checkout_overwrites_untracked_under_ignored_dir() {
	// git treats a file as ignored when an *ancestor directory* is ignored (it never descends into an
	// ignored directory), so a non-force checkout overwrites an untracked `foo/bar` when `foo/` is
	// ignored — the file is expendable (probed vs git 2.55: it overwrites without error). The overwrite
	// guard must match: matching only the leaf `foo/bar` against a dir-only `foo/` rule would miss it and
	// wrongly refuse.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("checkout-ignored-ancestor");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Commit A tracks foo/bar.
	std::fs::create_dir_all(work.join("foo")).unwrap();
	std::fs::write(work.join("foo/bar"), b"TRACKED\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "with-foo");
	let with_foo = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Commit B removes foo/ and ignores it.
	git(&["-C", w, "rm", "-q", "-r", "foo"]);
	std::fs::write(work.join(".gitignore"), b"foo/\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "ignore-foo");
	// An untracked foo/bar sits on disk — ignored via its ancestor `foo/`.
	std::fs::create_dir_all(work.join("foo")).unwrap();
	std::fs::write(work.join("foo/bar"), b"LOCAL\n").unwrap();

	let wt = make_repo(&work);
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&with_foo).unwrap())
		.await
		.unwrap();
	// A non-force checkout must NOT refuse — the untracked file is ignored (ancestor `foo/`), so expendable.
	wt.checkout(tree_a, false, None).await.unwrap();
	assert_eq!(
		std::fs::read(work.join("foo/bar")).unwrap(),
		b"TRACKED\n",
		"an untracked file under an ignored ancestor must be overwritten, matching git"
	);

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

/// Whether the OS temp filesystem is case-INSENSITIVE (macOS APFS, Windows) — where `Foo` and `foo`
/// share one inode. The hand-crafted case-colliding tests below require that (they stage both spellings and
/// rely on the single shared working file); on a case-SENSITIVE filesystem (e.g. Linux CI) the scenario
/// cannot exist, so those tests skip rather than fail.
fn case_insensitive_fs() -> bool {
	let dir = unique_tmp("case-probe");
	std::fs::create_dir_all(&dir).unwrap();
	std::fs::write(dir.join("CaseProbe"), b"x").unwrap();
	let insensitive = dir.join("caseprobe").exists();
	std::fs::remove_dir_all(&dir).ok();
	insensitive
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

/// A crafted index entry must never let a checkout delete a file outside the work tree (the
/// checkout CVE class): the removal loop's path guard refuses `../victim` and the outside file
/// survives.
#[tokio::test]
async fn checkout_refuses_a_traversal_index_entry() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-traversal");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let head = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

	let wt = make_repo(&work);
	let tree = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&head).unwrap())
		.await
		.unwrap();

	// A file just outside the work tree, and an index entry crafted to unlink it on checkout.
	let victim = work.parent().unwrap().join("victim-checkout-traversal");
	std::fs::write(&victim, b"do not delete\n").unwrap();
	let mut index = wt.load_index().await.unwrap();
	index.upsert(IndexEntry {
		stat: Stat::default(),
		mode: 0o100644,
		oid: ObjectId::<Sha256>::from_hex(&head).unwrap(),
		stage: 0,
		assume_valid: false,
		skip_worktree: false,
		intent_to_add: false,
		path: "../victim-checkout-traversal".to_owned(),
	});
	wt.save_index(&index).await.unwrap();

	// `../victim…` is absent from the target tree, so checkout's removal loop would unlink it —
	// but the path guard refuses, leaving the outside file untouched.
	assert!(matches!(
		wt.checkout(tree, true, None).await,
		Err(WorktreeError::UnsafePath(_))
	));
	assert!(
		victim.exists(),
		"a checkout must never delete a file outside the work tree"
	);

	std::fs::remove_file(&victim).ok();
	std::fs::remove_dir_all(&work).ok();
}

/// A held `index.lock` must abort a checkout *before* it mutates the working tree — the lock is
/// taken up front — so the tree is never left inconsistent with an index that could not be written.
#[tokio::test]
async fn checkout_aborts_before_mutating_on_a_held_index_lock() {
	if !git_supports_sha256() {
		return;
	}
	let (work, first) = two_commits("checkout-locked");
	// The work tree currently holds the second commit's a.txt (A2); tree1 would restore A1.
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&first).unwrap())
		.await
		.unwrap();

	// Another holder owns the index lock.
	let lock_path = work.join(".git").join("index.lock");
	std::fs::write(&lock_path, b"").unwrap();

	assert!(matches!(
		wt.checkout(tree1, true, None).await,
		Err(WorktreeError::IndexLocked)
	));
	assert_eq!(
		std::fs::read(work.join("a.txt")).unwrap(),
		b"A2\n",
		"a locked index must abort checkout before it mutates the working tree"
	);
	// The other holder's lock is left intact by the aborted attempt.
	assert!(lock_path.exists());

	std::fs::remove_dir_all(&work).ok();
}

/// The counterpart to the abort-on-held-lock test: a *successful* index write through the
/// FileStore-backed lock path (`lock_index` → `write_path_replace("index")` → release) must leave
/// `index.lock` gone and no temp residue behind, and must let the lock be re-acquired. Runs without
/// a git binary — it builds the repo natively — so it exercises the release half unconditionally.
#[tokio::test]
async fn a_successful_index_write_releases_the_lock() {
	let work = unique_tmp("index-release");
	let git_dir = work.join(".git");
	std::fs::create_dir_all(&git_dir).unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();
	let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir);

	std::fs::write(work.join("f.txt"), b"hi\n").unwrap();
	wt.add(&["f.txt"], "", false, None).await.unwrap();

	// The lock is released and the atomic-replace temp is cleaned up.
	assert!(
		!git_dir.join("index.lock").exists(),
		"index.lock must be released after a successful write"
	);
	let residue: Vec<String> = std::fs::read_dir(&git_dir)
		.unwrap()
		.filter_map(|entry| entry.ok())
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.filter(|name| name.starts_with(".tmp."))
		.collect();
	assert!(
		residue.is_empty(),
		"no temp residue in the git dir: {residue:?}"
	);

	// The write landed and the lock can be taken again (a second write succeeds).
	assert!(
		wt.load_index()
			.await
			.unwrap()
			.entries
			.iter()
			.any(|entry| entry.path == "f.txt")
	);
	wt.add(&["f.txt"], "", false, None).await.unwrap();

	std::fs::remove_dir_all(&work).ok();
}

/// A removal must not follow a symlinked ancestor out of the work tree: `link/x` is lexically safe
/// but `link` may point outside, so the guard refuses it and the outside file survives.
#[tokio::test]
async fn checkout_refuses_removal_through_a_symlinked_ancestor() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-symlink-ancestor");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let head = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

	// A directory outside the work tree holding a victim file, and a symlink `link` -> it inside.
	let outside = unique_tmp("checkout-symlink-outside");
	std::fs::create_dir_all(&outside).unwrap();
	std::fs::write(outside.join("victim"), b"do not delete\n").unwrap();
	std::os::unix::fs::symlink(&outside, work.join("link")).unwrap();

	let wt = make_repo(&work);
	let tree = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&head).unwrap())
		.await
		.unwrap();

	// A crafted `link/victim` entry (absent from the target) would be unlinked through the symlink;
	// the removal declines to follow the symlinked ancestor instead.
	let mut index = wt.load_index().await.unwrap();
	index.upsert(IndexEntry {
		stat: Stat::default(),
		mode: 0o100644,
		oid: ObjectId::<Sha256>::from_hex(&head).unwrap(),
		stage: 0,
		assume_valid: false,
		skip_worktree: false,
		intent_to_add: false,
		path: "link/victim".to_owned(),
	});
	wt.save_index(&index).await.unwrap();

	// Checkout completes (dropping the crafted entry) without following the symlink, so the outside
	// file survives.
	wt.checkout(tree, true, None).await.unwrap();
	assert!(
		outside.join("victim").exists(),
		"a checkout must not delete through a symlinked ancestor"
	);

	std::fs::remove_dir_all(&outside).ok();
	std::fs::remove_dir_all(&work).ok();
}

/// A branch switch that replaces a tracked directory with a symlink must complete: checkout writes
/// the parent symlink, then prunes the now-stale child under it *without* following the new symlink
/// (regression: the removal guard once aborted on the just-written symlink).
#[tokio::test]
async fn checkout_switches_a_directory_to_a_symlink() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("checkout-dir-to-symlink");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Tree A: `dir` is a directory with a child.
	std::fs::create_dir(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/file"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Tree B: `dir` is a symlink.
	std::fs::remove_dir_all(work.join("dir")).unwrap();
	std::os::unix::fs::symlink("elsewhere", work.join("dir")).unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");
	let b = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

	// Back to A (directory) via git, so the gitana checkout to B is a clean dir→symlink switch.
	git(&["-C", w, "reset", "--hard", &a]);
	assert!(work.join("dir").is_dir());

	let wt = make_repo(&work);
	let tree_b = wt
		.repository()
		.commit_tree(ObjectId::<Sha256>::from_hex(&b).unwrap())
		.await
		.unwrap();
	// The stale `dir/file` is pruned cleanly (without following the just-written `dir` symlink).
	wt.checkout(tree_b, true, None).await.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("dir"))
			.unwrap()
			.is_symlink(),
		"dir must now be a symlink"
	);
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "dir");

	std::fs::remove_dir_all(&work).ok();
}
