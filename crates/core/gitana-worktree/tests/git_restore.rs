#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::ObjectId;
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use gitana_worktree::{IndexEntry, Stat, WorkTree, WorktreeError};

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::new(LocalFileStore::new(&git_dir)));
	WorkTree::new(repo, work, git_dir)
}

/// A work-tree-escaping pathspec unique to `work` (`../<work-name>-escape.txt`) and the absolute
/// path it resolves to. The name is derived from the (process-unique) work-dir name, so a
/// regression that actually writes it cannot collide with another test sharing the temp parent.
/// The up-front removal clears only this exact path — a leftover from an earlier crash at the
/// same work-dir name — not every prior run's escape file, which is fine: distinct runs get
/// distinct names and so never alias.
fn escape_path(work: &std::path::Path) -> (String, PathBuf) {
	let name = format!("{}-escape.txt", work.file_name().unwrap().to_string_lossy());
	let outside = work.parent().unwrap().join(&name);
	let _ = std::fs::remove_file(&outside);
	(format!("../{name}"), outside)
}

/// Build two commits; return (work dir, first commit id). The working tree and index are
/// left at the second commit: `a.txt`=A2, `c.txt`=C, `sub/b.txt` removed.
fn two_commits(tag: &str) -> (PathBuf, String) {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/b.txt"), b"B\n").unwrap();
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
async fn restore_from_index_discards_worktree_changes() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let (work, _first) = two_commits("restore-index");
	let w = work.to_str().unwrap();
	let wt = make_repo(&work);

	// Dirty one tracked file and delete another; both are recoverable from the index.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	std::fs::remove_file(work.join("c.txt")).unwrap();

	wt.restore(None, true, false, &["a.txt", "c.txt"], "").await.unwrap();

	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");
	assert_eq!(std::fs::read(work.join("c.txt")).unwrap(), b"C\n");
	// The worktree now matches the index for these paths (nothing unstaged).
	assert!(git(&["-C", w, "diff", "--name-only"]).is_empty());

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_from_tree_updates_index_and_worktree() {
	if !git_supports_sha256() {
		return;
	}
	let (work, first) = two_commits("restore-tree");
	let w = work.to_str().unwrap();
	let wt = make_repo(&work);
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&first).unwrap())
		.await
		.unwrap();

	// Restore a single file and a directory (recreating a file deleted since the first commit).
	wt.restore(Some(tree1), true, true, &["a.txt", "sub"], "")
		.await
		.unwrap();

	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert_eq!(std::fs::read(work.join("sub/b.txt")).unwrap(), b"B\n");
	// Worktree matches index (restore staged the tree content too)...
	assert!(git(&["-C", w, "diff", "--name-only"]).is_empty());
	// ...and the index now differs from HEAD for exactly the restored paths.
	let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
	let mut names: Vec<&str> = staged.lines().collect();
	names.sort_unstable();
	assert_eq!(names, vec!["a.txt", "sub/b.txt"]);
	// Paths outside the pathspec are untouched (no pruning).
	assert_eq!(std::fs::read(work.join("c.txt")).unwrap(), b"C\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_dot_restores_every_index_entry() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-dot");
	let wt = make_repo(&work);

	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	std::fs::remove_file(work.join("c.txt")).unwrap();

	wt.restore(None, true, false, &["."], "").await.unwrap();

	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");
	assert_eq!(std::fs::read(work.join("c.txt")).unwrap(), b"C\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_strips_leading_dot_slash() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-dotslash");
	let wt = make_repo(&work);

	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	wt.restore(None, true, false, &["./a.txt"], "").await.unwrap();
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_with_prefix_is_relative_to_subdirectory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("restore-prefix");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"ROOT\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/a.txt"), b"SUB\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let wt = make_repo(&work);

	// From the `sub` directory, `a.txt` means `sub/a.txt`, not the root file.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	std::fs::write(work.join("sub/a.txt"), b"dirty\n").unwrap();
	wt.restore(None, true, false, &["a.txt"], "sub").await.unwrap();
	assert_eq!(std::fs::read(work.join("sub/a.txt")).unwrap(), b"SUB\n");
	assert_eq!(
		std::fs::read(work.join("a.txt")).unwrap(),
		b"dirty\n",
		"the root file is left untouched"
	);

	// From `sub`, `.` restores only entries under `sub/`.
	std::fs::write(work.join("sub/a.txt"), b"dirty\n").unwrap();
	wt.restore(None, true, false, &["."], "sub").await.unwrap();
	assert_eq!(std::fs::read(work.join("sub/a.txt")).unwrap(), b"SUB\n");
	assert_eq!(
		std::fs::read(work.join("a.txt")).unwrap(),
		b"dirty\n",
		"the root file is still untouched"
	);

	// From `sub`, `../a.txt` resolves to the root file (git accepts parent-relative specs).
	wt.restore(None, true, false, &["../a.txt"], "sub").await.unwrap();
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"ROOT\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_pathspec_above_root() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-above-root");
	let wt = make_repo(&work);

	// `..` from the root climbs outside the repository.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	assert!(matches!(
		wt.restore(None, true, false, &["../a.txt"], "").await,
		Err(WorktreeError::UnsafePath(_))
	));
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_empty_pathspec() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-empty");
	let wt = make_repo(&work);

	// An empty pathspec must not silently restore everything.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	assert!(matches!(
		wt.restore(None, true, false, &[""], "").await,
		Err(WorktreeError::EmptyPathspec)
	));
	assert_eq!(
		std::fs::read(work.join("a.txt")).unwrap(),
		b"dirty\n",
		"the dirty file is left untouched"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_absolute_pathspec() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-abs");
	let wt = make_repo(&work);

	// A leading-`/` pathspec must not be silently relativised into a tracked file.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	for spec in ["/a.txt", "/"] {
		assert!(matches!(
			wt.restore(None, true, false, &[spec], "").await,
			Err(WorktreeError::AbsolutePathspec(_))
		));
	}
	assert_eq!(
		std::fs::read(work.join("a.txt")).unwrap(),
		b"dirty\n",
		"the dirty file is left untouched"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_handles_file_directory_type_changes() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("restore-typechange");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Commit A: `thing` is a file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Commit B: `thing` is a directory.
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");
	let wt = make_repo(&work);

	// Case 1: index has `thing/child.txt`, but the working tree was replaced with a file
	// `thing`. Restoring from the index removes the file and recreates the directory.
	std::fs::remove_dir_all(work.join("thing")).unwrap();
	std::fs::write(work.join("thing"), b"STRAY\n").unwrap();
	wt.restore(None, true, false, &["thing"], "").await.unwrap();
	assert!(work.join("thing").is_dir());
	assert_eq!(
		std::fs::read(work.join("thing/child.txt")).unwrap(),
		b"CHILD\n"
	);

	// Case 2: restoring from commit A (where `thing` is a file) replaces the directory with
	// the file and drops the stale `thing/child.txt` index entry.
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&a).unwrap())
		.await
		.unwrap();
	wt.restore(Some(tree_a), true, true, &["thing"], "")
		.await
		.unwrap();
	assert!(
		std::fs::symlink_metadata(work.join("thing"))
			.unwrap()
			.is_file()
	);
	assert_eq!(std::fs::read(work.join("thing")).unwrap(), b"FILE\n");
	assert!(!work.join("thing/child.txt").exists());
	assert_eq!(git(&["-C", w, "ls-files"]).trim(), "thing");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_trailing_slash_on_file() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-trailing-slash");
	let wt = make_repo(&work);

	// `a.txt/` and `a.txt/.` require a directory; `a.txt` is a file, so git (and we) reject
	// them and leave the dirty content alone instead of silently restoring it.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	for spec in ["a.txt/", "a.txt/."] {
		assert!(matches!(
			wt.restore(None, true, false, &[spec], "").await,
			Err(WorktreeError::PathspecMatch(_))
		));
	}
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	// `a.txt/..` resolves to the parent directory, which git accepts (restores everything).
	wt.restore(None, true, false, &["a.txt/.."], "").await.unwrap();
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_unmatched_pathspec() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-nomatch");
	let wt = make_repo(&work);

	assert!(matches!(
		wt.restore(None, true, false, &["nope.txt"], "").await,
		Err(WorktreeError::PathspecMatch(_))
	));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn staged_restore_unstages_a_new_file() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-staged-unstage");
	let w = work.to_str().unwrap();
	let wt = make_repo(&work);

	// Stage a brand-new file: in the index, but absent from HEAD.
	std::fs::write(work.join("new.txt"), b"NEW\n").unwrap();
	git(&["-C", w, "add", "new.txt"]);
	assert_eq!(
		git(&["-C", w, "diff", "--cached", "--name-only"]).trim(),
		"new.txt"
	);

	// Restoring the index entry from HEAD (which lacks it) drops it from the index without
	// touching the working-tree file — the everyday `git restore --staged` unstage.
	let head_tree = wt.repository().rev_parse("HEAD^{tree}").await.unwrap();
	wt.restore(Some(head_tree), false, true, &["new.txt"], "")
		.await
		.unwrap();

	assert!(git(&["-C", w, "diff", "--cached", "--name-only"]).is_empty());
	assert_eq!(
		std::fs::read(work.join("new.txt")).unwrap(),
		b"NEW\n",
		"the working-tree file is left in place"
	);
	assert_eq!(
		git(&["-C", w, "status", "--porcelain", "new.txt"]).trim(),
		"?? new.txt",
		"the file is now untracked"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn staged_restore_resets_index_to_head() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-staged-reset");
	let w = work.to_str().unwrap();
	let wt = make_repo(&work);

	// Stage an edit to a tracked file (HEAD has A2).
	std::fs::write(work.join("a.txt"), b"STAGED\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
	assert_eq!(
		git(&["-C", w, "diff", "--cached", "--name-only"]).trim(),
		"a.txt"
	);

	// Restore the index entry from HEAD: the staged change is dropped, but the working-tree edit
	// remains (now an unstaged modification).
	let head_tree = wt.repository().rev_parse("HEAD^{tree}").await.unwrap();
	wt.restore(Some(head_tree), false, true, &["a.txt"], "")
		.await
		.unwrap();

	assert!(
		git(&["-C", w, "diff", "--cached", "--name-only"]).is_empty(),
		"nothing staged after resetting the index to HEAD"
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"STAGED\n");
	assert_eq!(
		git(&["-C", w, "diff", "--name-only"]).trim(),
		"a.txt",
		"the working-tree edit is now unstaged"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn worktree_restore_from_tree_deletes_absent_path() {
	if !git_supports_sha256() {
		return;
	}
	let (work, first) = two_commits("restore-wt-delete");
	let w = work.to_str().unwrap();
	let wt = make_repo(&work);
	// The first commit's tree has `a.txt` and `sub/b.txt`, but no `c.txt`.
	let tree1 = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&first).unwrap())
		.await
		.unwrap();
	assert!(work.join("c.txt").exists());

	// A worktree-only restore from a tree that lacks `c.txt` deletes the working-tree file and
	// leaves the index entry alone (so it shows as an unstaged deletion).
	wt.restore(Some(tree1), true, false, &["c.txt"], "")
		.await
		.unwrap();

	assert!(!work.join("c.txt").exists(), "the working-tree file is deleted");
	assert_eq!(
		git(&["-C", w, "diff", "--name-only"]).trim(),
		"c.txt",
		"the deletion is unstaged"
	);
	assert!(
		git(&["-C", w, "diff", "--cached", "--name-only"]).is_empty(),
		"the index is untouched by a worktree-only restore"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn staged_restore_rejects_unsafe_tree_path() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-staged-unsafe");
	let wt = make_repo(&work);

	// A crafted tree whose flattened entry escapes the work tree. A staged-only restore upserts
	// index entries without going through the working-tree write that normally validates paths,
	// so this must still be refused.
	let (escape_spec, escape_outside) = escape_path(&work);
	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	let hostile = wt
		.repository()
		.write_tree(&[TreeBuildEntry {
			path: escape_spec,
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();

	let before = wt.load_index().unwrap();
	assert!(matches!(
		wt.restore(Some(hostile), false, true, &["."], "").await,
		Err(WorktreeError::UnsafePath(_))
	));
	// The index is untouched, and nothing was written outside the work tree.
	assert_eq!(wt.load_index().unwrap(), before);
	assert!(!escape_outside.exists());

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn worktree_restore_from_unsafe_tree_leaves_filesystem_unchanged() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-wt-unsafe");
	let wt = make_repo(&work);

	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	let hostile = wt
		.repository()
		.write_tree(&[TreeBuildEntry {
			path: ".git/config".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();

	let config_before = std::fs::read(work.join(".git/config")).unwrap();
	assert!(matches!(
		wt.restore(Some(hostile), true, false, &["."], "").await,
		Err(WorktreeError::UnsafePath(_))
	));
	// The real `.git/config` was not overwritten by the hostile entry.
	assert_eq!(std::fs::read(work.join(".git/config")).unwrap(), config_before);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn restore_rejects_unsafe_index_path() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _first) = two_commits("restore-index-unsafe");
	let wt = make_repo(&work);

	// Inject a hostile entry directly into the index, as a corrupt or hostile index might carry.
	let (escape_spec, escape_outside) = escape_path(&work);
	let mut index = wt.load_index().unwrap();
	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	index.upsert(IndexEntry {
		stat: Stat::default(),
		mode: 0o100644,
		oid: blob,
		stage: 0,
		assume_valid: false,
		path: escape_spec,
	});
	wt.save_index(&index).unwrap();

	// Selecting it via `.` must be refused before the working-tree file is written.
	assert!(matches!(
		wt.restore(None, true, false, &["."], "").await,
		Err(WorktreeError::UnsafePath(_))
	));
	assert!(!escape_outside.exists());

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn staged_restore_cleans_type_conflicts() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("restore-staged-typechange");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// Commit A: `thing` is a file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "A");
	let a = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
	// Commit B: `thing` is a directory, so the index now holds `thing/child.txt`.
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "B");
	let wt = make_repo(&work);

	// A staged-only restore from commit A (where `thing` is a blob) must drop the stale
	// `thing/child.txt` index entry and stage `thing`, without touching the working tree.
	let tree_a = wt
		.repository()
		.commit_tree(ObjectId::from_hex(&a).unwrap())
		.await
		.unwrap();
	wt.restore(Some(tree_a), false, true, &["thing"], "")
		.await
		.unwrap();

	assert_eq!(
		git(&["-C", w, "ls-files"]).trim(),
		"thing",
		"the index records `thing` as a blob, with no `thing/child.txt`"
	);
	// The working tree still holds the directory (staged-only restore left it alone).
	assert!(work.join("thing").is_dir());
	assert_eq!(
		std::fs::read(work.join("thing/child.txt")).unwrap(),
		b"CHILD\n"
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

fn git_supports_sha256() -> bool {
	// Probe once per test binary: every test calls this, and a shared probe dir raced under
	// load. `OnceLock` makes it concurrency-safe and spawns `git init` a single time.
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-restore");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
