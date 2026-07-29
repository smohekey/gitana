#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{IndexEntry, Stat, WorkTree, WorktreeError};

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

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
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&paths, "")
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

/// `add` stages a glob pathspec byte-for-byte like `git add '*.rs'` — the wildcard crosses `/`, so
/// every `.rs` at any depth is staged and the `.txt` is left. (git receives the literal `*.rs`; the
/// args array never goes through a shell.)
#[tokio::test]
async fn add_stages_a_glob_like_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-glob");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("src/sub")).unwrap();
	std::fs::write(work.join("src/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("src/sub/b.rs"), b"2\n").unwrap();
	std::fs::write(work.join("src/c.txt"), b"3\n").unwrap();
	std::fs::write(work.join("top.rs"), b"4\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["*.rs"], "")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", w, "add", "*.rs"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs, "glob add must stage identically to git");
	assert!(ours.iter().any(|l| l.ends_with("\tsrc/a.rs")));
	assert!(ours.iter().any(|l| l.ends_with("\tsrc/sub/b.rs")));
	assert!(ours.iter().any(|l| l.ends_with("\ttop.rs")));
	assert!(!ours.iter().any(|l| l.ends_with("\tsrc/c.txt")));

	std::fs::remove_dir_all(&work).ok();
}

/// `add . ':(exclude)vendor'` stages everything except the excluded subtree, byte-for-byte like git.
#[tokio::test]
async fn add_excludes_a_negative_pathspec_like_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-exclude");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::create_dir_all(work.join("vendor")).unwrap();
	std::fs::write(work.join("src/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("vendor/v.rs"), b"2\n").unwrap();
	std::fs::write(work.join("top.rs"), b"3\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&[".", ":(exclude)vendor"], "")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", w, "add", ".", ":(exclude)vendor"]);
	let theirs = ls_files(w);

	assert_eq!(
		ours, theirs,
		"negative pathspec must stage identically to git"
	);
	assert!(!ours.iter().any(|l| l.ends_with("\tvendor/v.rs")));
	assert!(ours.iter().any(|l| l.ends_with("\tsrc/a.rs")));
	assert!(ours.iter().any(|l| l.ends_with("\ttop.rs")));

	std::fs::remove_dir_all(&work).ok();
}

/// `add ':(icase)SRC/B.RS'` records the *actual* worktree path (`src/b.rs`), not the pathspec's
/// spelling — the case-folded match resolves to the real file, as git does.
#[tokio::test]
async fn add_icase_resolves_to_the_actual_worktree_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-icase");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/b.rs"), b"B\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&[":(icase)SRC/B.RS"], "")
		.await
		.unwrap();
	let ours = ls_files(w);

	assert!(
		ours.iter().any(|l| l.ends_with("\tsrc/b.rs")),
		"index should record the real path, got {ours:?}"
	);
	assert!(!ours.iter().any(|l| l.ends_with("\tSRC/B.RS")));

	std::fs::remove_dir_all(&work).ok();
}

/// A glob that walks a subdirectory still applies an ancestor `.gitignore` — `add 'src/*.rs'` skips a
/// file the root `.gitignore` excludes, byte-for-byte like git.
#[tokio::test]
async fn add_glob_applies_ancestor_gitignore() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-glob-ignore");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"ignored.rs\n").unwrap();
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/ignored.rs"), b"x\n").unwrap();
	std::fs::write(work.join("src/keep.rs"), b"y\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["src/*.rs"], "")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", w, "add", "src/*.rs"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs, "ancestor .gitignore must be honoured");
	assert!(ours.iter().any(|l| l.ends_with("\tsrc/keep.rs")));
	assert!(!ours.iter().any(|l| l.ends_with("\tsrc/ignored.rs")));

	std::fs::remove_dir_all(&work).ok();
}

/// `add '*.rs' ':!*.rs'` is a no-op success — git counts the positive as matched before subtracting
/// the exclusion, so it is not "did not match".
#[tokio::test]
async fn add_positive_then_excluded_is_a_noop() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-exclude-noop");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.rs"), b"1\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["*.rs", ":!*.rs"], "")
		.await
		.unwrap();
	assert!(ls_files(w).is_empty(), "nothing staged, but no error");

	std::fs::remove_dir_all(&work).ok();
}

/// A glob whose *only* candidate is a tracked path deleted from the working tree, then excluded by a
/// negative — `add '*.rs' ':!*.rs'` after staging then deleting `a.rs` — counts the positive as
/// matched before the exclusion, so it is a no-op success that does NOT stage the deletion (probed vs
/// git 2.50.1). Without separating match accounting from exclusion, this errored `PathspecMatch`.
#[tokio::test]
async fn add_glob_tracked_deletion_then_excluded_is_a_noop() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-glob-del-exclude");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.rs"), b"1\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	// Stage a.rs, then delete it: it is now a tracked (stage-0) candidate with no on-disk file.
	open().add(&["a.rs"], "").await.unwrap();
	std::fs::remove_file(work.join("a.rs")).unwrap();

	open().add(&["*.rs", ":!*.rs"], "").await.unwrap();
	// The deletion must NOT be staged — a.rs remains a tracked entry, exactly as git leaves it.
	assert!(
		ls_files(w).iter().any(|l| l.ends_with("\ta.rs")),
		"deletion wrongly staged: {:?}",
		ls_files(w)
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A glob whose literal base directory is ignored by an ancestor `.gitignore` matches no *untracked*
/// file under it — `add 'ignored/*.rs'` with `.gitignore` containing `ignored/` is git's "did not
/// match any files" (probed vs git 2.50.1), not a stage of the ignored file.
#[tokio::test]
async fn add_glob_under_ignored_base_does_not_match() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-glob-ignored-base");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"ignored/\n").unwrap();
	std::fs::create_dir_all(work.join("ignored")).unwrap();
	std::fs::write(work.join("ignored/a.rs"), b"x\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let result = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["ignored/*.rs"], "")
		.await;
	assert!(
		matches!(result, Err(WorktreeError::PathspecMatch(_))),
		"expected PathspecMatch, got {result:?}"
	);
	assert!(ls_files(w).is_empty(), "nothing staged");

	std::fs::remove_dir_all(&work).ok();
}

/// A modification to an already-tracked file under an ignored directory is staged by the broad
/// walk-based `add` forms — `add .` and a negative-only `add :!x` — exactly like git, which never
/// applies ignore rules to a tracked path. Each form is checked against a fresh `git add` of the same
/// pathspec as the oracle. (Explicitly naming the ignored directory, `add ignored`, is git's
/// ignored-path advice path — exit 1 — which gitana does not yet implement, so it is out of scope.)
#[tokio::test]
async fn add_restages_tracked_file_under_ignored_dir_like_git() {
	if !git_supports_sha256() {
		return;
	}
	for spec in [".", ":!nope"] {
		let work = unique_tmp("add-tracked-ignored");
		let git_dir = work.join(".git");
		let w = work.to_str().unwrap();
		git(&["init", "--object-format=sha256", "-q", w]);
		std::fs::write(work.join(".gitignore"), b"ignored/\n").unwrap();
		std::fs::create_dir_all(work.join("ignored")).unwrap();
		std::fs::write(work.join("ignored/tracked.rs"), b"orig\n").unwrap();
		// Force-add the tracked file past the ignore rule, then commit the baseline.
		git(&["-C", w, "add", "-f", "ignored/tracked.rs", ".gitignore"]);
		git(&[
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"base",
		]);
		// Modify the tracked-but-ignored file.
		std::fs::write(work.join("ignored/tracked.rs"), b"changed\n").unwrap();

		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.add(&[spec], "")
			.await
			.unwrap();
		let ours = ls_files(w);

		// Oracle: reset the index to HEAD and run the same `git add <spec>`.
		git(&["-C", w, "reset", "-q"]);
		git(&["-C", w, "add", spec]);
		let theirs = ls_files(w);

		assert_eq!(
			ours, theirs,
			"`add {spec}` must match git for a tracked file under an ignored dir"
		);
		// And it really staged the modification (not the committed blob).
		let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
		assert!(
			staged.lines().any(|l| l == "ignored/tracked.rs"),
			"`add {spec}` staged the modification: {staged:?}"
		);

		std::fs::remove_dir_all(&work).ok();
	}
}

/// A negative-only `add ':!x'` resolves a deleted unmerged path — its higher-stage (1/2/3) entries are
/// dropped, recording the conflict resolution — because the implicit `.` reconciles unmerged paths, not
/// only stage-0 ones (probed vs git 2.50.1: `add :!nope` clears a removed unmerged path). The conflict
/// is crafted directly in the index, then the working-tree file is left absent.
#[tokio::test]
async fn add_negative_only_resolves_deleted_unmerged_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-unmerged-del");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir);

	// Craft an unmerged `conflict` path (stages 1/2/3, no stage 0) whose working-tree file is absent.
	let mut index = wt.load_index().await.unwrap();
	let blob = wt.repository().write_blob(b"c\n").await.unwrap();
	for stage in 1..=3 {
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid: blob,
			stage,
			assume_valid: false,
			skip_worktree: false,
			path: "conflict".to_owned(),
		});
	}
	wt.save_index(&index).await.unwrap();

	wt.add(&[":!nope"], "").await.unwrap();

	// The deletion is resolved: no `conflict` entry of any stage remains.
	let after = wt.load_index().await.unwrap();
	assert!(
		!after.entries.iter().any(|e| e.path == "conflict"),
		"unmerged deletion should be resolved"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A literal `add conflict` naming a deleted unmerged path resolves it — clearing its stage-1/2/3
/// entries and recording the deletion — rather than reporting "did not match" (probed vs git 2.50.1).
/// The `matched` check must count unmerged exact/descendant paths, not only stage-0 entries.
#[tokio::test]
async fn add_literal_resolves_deleted_unmerged_path() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-lit-unmerged-del");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir);
	let mut index = wt.load_index().await.unwrap();
	let blob = wt.repository().write_blob(b"c\n").await.unwrap();
	for stage in 1..=3 {
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid: blob,
			stage,
			assume_valid: false,
			skip_worktree: false,
			path: "conflict".to_owned(),
		});
	}
	wt.save_index(&index).await.unwrap();

	// A literal add of the unmerged path (absent on disk) resolves it, not a PathspecMatch error.
	wt.add(&["conflict"], "").await.unwrap();
	let after = wt.load_index().await.unwrap();
	assert!(!after.entries.iter().any(|e| e.path == "conflict"));

	std::fs::remove_dir_all(&work).ok();
}

/// Explicitly naming an ignored directory (`add ignored`) stages the modified TRACKED child but NOT the
/// untracked ignored sibling — git refuses untracked ignored files (its "paths are ignored, use -f"
/// advice), staging only the tracked modifications (probed vs git 2.50.1). gitana does not yet emit the
/// advisory exit code (a documented follow-up), but its staged result must match git's.
#[tokio::test]
async fn add_explicit_ignored_dir_stages_only_tracked() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-ignored-dir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("ignored")).unwrap();
	std::fs::write(work.join("ignored/tracked.rs"), b"1\n").unwrap();
	git(&["-C", w, "add", "-f", "ignored/tracked.rs"]);
	std::fs::write(work.join(".gitignore"), b"ignored/\n").unwrap();
	git(&["-C", w, "add", ".gitignore"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	// Modify the tracked child, add an untracked ignored sibling.
	std::fs::write(work.join("ignored/tracked.rs"), b"changed\n").unwrap();
	std::fs::write(work.join("ignored/new.rs"), b"n\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["ignored"], "")
		.await
		.unwrap();

	// The tracked modification is staged; the untracked ignored file is NOT (git refuses it).
	let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
	assert_eq!(
		staged.trim(),
		"ignored/tracked.rs",
		"only the tracked mod: {staged:?}"
	);
	assert!(
		!git(&["-C", w, "ls-files"])
			.lines()
			.any(|l| l == "ignored/new.rs"),
		"untracked ignored file must not be staged"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// An absent positive literal that matches no tracked path is git's "did not match", and a negative
/// pathspec does NOT suppress that error — `add 'missing/' ':!missing'` fails, and so does a bare
/// `add missing`. Probed vs git 2.50.1 (exit 128). The exclusion only gates staging *after* the
/// positive has matched something.
#[tokio::test]
async fn add_absent_positive_still_errors_despite_exclusion() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-absent-excluded");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("present"), b"p\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	// Excluded absent positive still errors.
	let excluded = open().add(&["missing/", ":!missing"], "").await;
	assert!(
		matches!(excluded, Err(WorktreeError::PathspecMatch(_))),
		"got {excluded:?}"
	);
	// A bare absent-untracked positive errors too.
	let bare = open().add(&["missing"], "").await;
	assert!(
		matches!(bare, Err(WorktreeError::PathspecMatch(_))),
		"got {bare:?}"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Under active sparse-checkout, a positive pathspec whose matches are ALL out-of-cone is refused with
/// the sparse error, even a glob (`add 'out/*'`) or an excluded explicit file (`add out/f :!out/f`) —
/// sparse validation precedes exclusion subtraction (probed vs git 2.50.1, which exits nonzero).
#[tokio::test]
async fn add_out_of_cone_matches_are_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-sparse-refuse");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("in")).unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("in/f"), b"1\n").unwrap();
	std::fs::write(work.join("out/f"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	// Enable cone sparse-checkout for `in/` only, then apply it (removes out-of-cone, sets skip-worktree).
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/in/\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().reapply_sparse().await.unwrap();
	assert!(
		!work.join("out/f").exists(),
		"out/f should be removed by sparse apply"
	);
	// Recreate the out-of-cone file on disk.
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("out/f"), b"2\n").unwrap();

	// A glob whose only matches are out-of-cone is refused.
	assert!(
		matches!(
			open().add(&["out/*"], "").await,
			Err(WorktreeError::SparsePathExcluded(_))
		),
		"add 'out/*' should be refused"
	);
	// An excluded explicit out-of-cone file is still refused (sparse check precedes exclusion).
	assert!(
		matches!(
			open().add(&["out/f", ":!out/f"], "").await,
			Err(WorktreeError::SparsePathExcluded(_))
		),
		"add out/f :!out/f should be refused"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Overlapping GLOB pathspecs that select the same deleted tracked path succeed — `add '*.rs' '*.rs'`
/// on a deleted `a.rs` stages the deletion, rather than the second glob reporting "did not match"
/// because the first already removed the entry. Match accounting uses the pre-staging snapshot (probed
/// vs git 2.50.1).
#[tokio::test]
async fn add_overlapping_globs_for_a_deleted_file_succeed() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-overlap-glob-del");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	std::fs::remove_file(work.join("a.rs")).unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["*.rs", "*.rs"], "")
		.await
		.unwrap();
	let files = ls_files(w);
	assert!(
		!files.iter().any(|l| l.ends_with("\ta.rs")),
		"deletion staged: {files:?}"
	);
	assert!(files.iter().any(|l| l.ends_with("\tkeep")));

	std::fs::remove_dir_all(&work).ok();
}

/// The sparse advice keys on the glob's BASE directory, not on individual matches: a broad glob (base
/// in-cone) silently skips a tracked skip-worktree entry it merely sweeps over, so `add '*.rs'` with a
/// modified in-cone `in/a.rs` and a recreated tracked out-of-cone `out/b.rs` stages `in/a.rs` and
/// SUCCEEDS. But an out-of-cone-based glob advises even when the matches are all excluded, so
/// `add 'out/*' ':!out/*'` still errors (sparse precedes exclusion). Probed vs git 2.50.1.
#[tokio::test]
async fn add_sparse_advice_keys_on_the_glob_base() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-sparse-base");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("in")).unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("in/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("out/b.rs"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/in/\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().reapply_sparse().await.unwrap();
	// Modify the in-cone file and recreate the tracked out-of-cone file on disk.
	std::fs::write(work.join("in/a.rs"), b"changed\n").unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("out/b.rs"), b"2\n").unwrap();

	// Broad glob: succeeds (out/b.rs is a tracked skip-worktree entry, silently skipped), stages in/a.rs.
	open().add(&["*.rs"], "").await.unwrap();
	let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
	assert!(
		staged.lines().any(|l| l == "in/a.rs"),
		"in-cone staged: {staged:?}"
	);
	assert!(
		!staged.lines().any(|l| l == "out/b.rs"),
		"out-of-cone not staged"
	);

	// Out-of-cone-based glob advises even when everything it matches is excluded.
	git(&["-C", w, "reset", "-q"]);
	let excl = open().add(&["out/*", ":!out/*"], "").await;
	assert!(
		matches!(excl, Err(WorktreeError::SparsePathExcluded(_))),
		"got {excl:?}"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Two sparse-advice refinements: (1) a broad `add .` over a MODIFIED tracked out-of-cone file whose
/// skip-worktree bit was cleared (still tracked) succeeds silently — the omission test is "no index
/// entry", not the bit; (2) an UNTRACKED out-of-cone file removed by a negative (`add out/new.rs
/// :!out/new.rs`) is an empty selection and succeeds — the exclusion applies before the untracked
/// sparse flag. Both probed vs git 2.50.1.
#[tokio::test]
async fn add_sparse_untracked_and_dirty_tracked_refinements() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-sparse-refine");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("in")).unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("in/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("out/f.rs"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/in/\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().reapply_sparse().await.unwrap();

	// (1) Recreate out/f.rs MODIFIED (reapply preserves it, clearing skip-worktree). Broad `add .`
	// silently skips it (still tracked) and succeeds, staging only the in-cone change.
	std::fs::write(work.join("in/a.rs"), b"changed\n").unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("out/f.rs"), b"dirty\n").unwrap();
	open().add(&["."], "").await.unwrap();
	let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
	assert!(
		staged.lines().any(|l| l == "in/a.rs"),
		"in-cone staged: {staged:?}"
	);
	assert!(
		!staged.lines().any(|l| l == "out/f.rs"),
		"dirty tracked out-of-cone not staged"
	);
	git(&["-C", w, "reset", "-q"]);

	// (2) An untracked out-of-cone file removed by a negative is an empty selection -> success.
	std::fs::write(work.join("out/new.rs"), b"n\n").unwrap();
	open()
		.add(&["out/new.rs", ":!out/new.rs"], "")
		.await
		.unwrap();

	std::fs::remove_dir_all(&work).ok();
}

/// A glob matching only tracked out-of-cone entries (absent from the working tree under sparse-checkout)
/// reports the sparse advice, not "did not match" — and `add 'in/*' 'out/*'` stages the in-cone glob's
/// work before surfacing the deferred error (probed vs git 2.50.1).
#[tokio::test]
async fn add_glob_matching_only_out_of_cone_tracked_reports_sparse() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-glob-oob-tracked");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("in")).unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("in/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("out/f.rs"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/in/\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().reapply_sparse().await.unwrap();
	assert!(
		!work.join("out/f.rs").exists(),
		"out/f.rs removed by sparse"
	);

	// The glob matches only the tracked, out-of-cone (absent) out/f.rs -> sparse advice, not "did not match".
	assert!(
		matches!(
			open().add(&["out/*"], "").await,
			Err(WorktreeError::SparsePathExcluded(_))
		),
		"add 'out/*' should report the sparse path"
	);

	// `add 'in/*' 'out/*'`: the in-cone glob's modification is staged (saved) before the deferred error.
	std::fs::write(work.join("in/a.rs"), b"changed\n").unwrap();
	let result = open().add(&["in/*", "out/*"], "").await;
	assert!(
		matches!(result, Err(WorktreeError::SparsePathExcluded(_))),
		"got {result:?}"
	);
	assert!(
		git(&["-C", w, "diff", "--cached", "--name-only"])
			.lines()
			.any(|l| l == "in/a.rs"),
		"in-cone change staged before the deferred error"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A broad pathspec that sweeps up an UNTRACKED out-of-cone file stages the in-cone changes, saves the
/// index, and THEN returns the deferred sparse error — git's "stage in-cone, exit nonzero with advice"
/// (probed vs git 2.50.1). `add '*.rs'` stages the in-cone `in/a.rs` yet still reports; `add ':!in'`
/// reports without staging anything out-of-cone.
#[tokio::test]
async fn add_mixed_sparse_stages_in_cone_then_defers_error() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-sparse-mixed");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("in")).unwrap();
	std::fs::write(work.join("in/a.rs"), b"1\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/in/\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().reapply_sparse().await.unwrap();
	// Modify the in-cone file and create a NEW untracked out-of-cone file.
	std::fs::write(work.join("in/a.rs"), b"changed\n").unwrap();
	std::fs::create_dir_all(work.join("out")).unwrap();
	std::fs::write(work.join("out/new.rs"), b"n\n").unwrap();

	// A broad glob: the in-cone modification is staged (saved), and the error is still surfaced.
	let result = open().add(&["*.rs"], "").await;
	assert!(
		matches!(result, Err(WorktreeError::SparsePathExcluded(_))),
		"got {result:?}"
	);
	let staged = git(&["-C", w, "diff", "--cached", "--name-only"]);
	assert!(
		staged.lines().any(|l| l == "in/a.rs"),
		"in-cone staged: {staged:?}"
	);
	assert!(!staged.lines().any(|l| l == "out/new.rs"));

	// A negative-only pathspec reports the omission too (nothing out-of-cone staged).
	git(&["-C", w, "reset", "-q"]);
	let neg = open().add(&[":!in"], "").await;
	assert!(
		matches!(neg, Err(WorktreeError::SparsePathExcluded(_))),
		"got {neg:?}"
	);
	assert!(
		!git(&["-C", w, "diff", "--cached", "--name-only"])
			.lines()
			.any(|l| l == "out/new.rs")
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Overlapping positive pathspecs that select the same deleted tracked file succeed — `add gone gone`
/// and `add . gone` stage the deletion once, rather than the second occurrence reporting "did not
/// match" because the first already removed the entry. The match check uses a pre-staging snapshot
/// (probed vs git 2.50.1).
#[tokio::test]
async fn add_overlapping_specs_for_a_deleted_file_succeed() {
	if !git_supports_sha256() {
		return;
	}
	for specs in [vec!["gone", "gone"], vec![".", "gone"]] {
		let work = unique_tmp("add-overlap-del");
		let git_dir = work.join(".git");
		let w = work.to_str().unwrap();
		git(&["init", "--object-format=sha256", "-q", w]);
		std::fs::write(work.join("gone"), b"g\n").unwrap();
		std::fs::write(work.join("keep"), b"k\n").unwrap();
		git(&["-C", w, "add", "-A"]);
		git(&[
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"base",
		]);
		std::fs::remove_file(work.join("gone")).unwrap();

		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.add(&specs, "")
			.await
			.unwrap_or_else(|e| panic!("add {specs:?} should succeed: {e:?}"));
		// The deletion is staged (gone no longer tracked); keep survives.
		let files = ls_files(w);
		assert!(
			!files.iter().any(|l| l.ends_with("\tgone")),
			"{specs:?}: {files:?}"
		);
		assert!(files.iter().any(|l| l.ends_with("\tkeep")));

		std::fs::remove_dir_all(&work).ok();
	}
}

/// When a tracked *file* `dir` is replaced on disk by a directory and its only child is excluded
/// (`add dir ':!dir/child'`), the stale `dir` file entry is still staged as a deletion — the
/// reconciliation admits the exact replaced path, not only `dir/` children (probed vs git 2.50.1).
#[tokio::test]
async fn add_reconciles_file_replaced_by_directory_with_excluded_child() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-file-to-dir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("dir"), b"file\n").unwrap();
	std::fs::write(work.join("keep"), b"k\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"base",
	]);
	std::fs::remove_file(work.join("dir")).unwrap();
	std::fs::create_dir(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/child"), b"c\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["dir", ":!dir/child"], "")
		.await
		.unwrap();
	// The old `dir` file entry is deleted; the excluded child stays untracked.
	let files = ls_files(w);
	assert!(
		!files.iter().any(|l| l.ends_with("\tdir")),
		"stale file entry: {files:?}"
	);
	assert!(!files.iter().any(|l| l.ends_with("\tdir/child")));
	assert!(files.iter().any(|l| l.ends_with("\tkeep")));

	std::fs::remove_dir_all(&work).ok();
}

/// A glob with an escaped separator (`dir\/foo`) stages `dir/foo` like git — the base-directory
/// derivation must not treat the backslash as part of a real directory name (`dir\`), which would skip
/// the walk and report the spec unmatched. Probed vs git 2.50.1 (`add 'dir\/foo'` stages `dir/foo`).
#[tokio::test]
async fn add_glob_with_escaped_separator_stages_the_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-escaped-sep");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/foo"), b"1\n").unwrap();
	std::fs::write(work.join("dir/bar"), b"2\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["dir\\/foo"], "")
		.await
		.unwrap();
	let ours = ls_files(w);

	assert!(
		ours.iter().any(|l| l.ends_with("\tdir/foo")),
		"got {ours:?}"
	);
	assert!(!ours.iter().any(|l| l.ends_with("\tdir/bar")));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_with_prefix_is_relative_to_subdirectory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-prefix");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"ROOT\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/a.txt"), b"SUB\n").unwrap();

	// From the `sub` directory, `a.txt` means `sub/a.txt`, like `git -C sub add a.txt`.
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["a.txt"], "sub")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", &format!("{w}/sub"), "add", "a.txt"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs);
	assert!(ours.iter().any(|l| l.ends_with("\tsub/a.txt")));
	assert!(
		!ours
			.iter()
			.any(|l| l.ends_with("\ta.txt") && !l.ends_with("\tsub/a.txt"))
	);

	// From `sub`, `../a.txt` resolves to the root file and is stored as `a.txt`, not
	// `sub/../a.txt`, matching `git -C sub add ../a.txt`.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.add(&["../a.txt"], "sub")
		.await
		.unwrap();
	let ours = ls_files(w);

	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", &format!("{w}/sub"), "add", "../a.txt"]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs);
	assert!(
		ours
			.iter()
			.any(|l| l.ends_with("\ta.txt") && !l.ends_with("\tsub/a.txt"))
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_trailing_slash_requires_a_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-trailing-slash");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"v1\n").unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("sub/x.txt"), b"s1\n").unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir);

	// `a.txt/` and `a.txt/.` name a file as a directory: git rejects them, and so do we.
	for spec in ["a.txt/", "a.txt/."] {
		assert!(matches!(
			wt.add(&[spec], "").await,
			Err(gitana_worktree::WorktreeError::PathspecMatch(_))
		));
		let git_ok = Command::new("git")
			.args(["-C", w, "add", spec])
			.output()
			.unwrap()
			.status
			.success();
		assert!(!git_ok, "git also rejects '{spec}'");
	}
	assert!(ls_files(w).is_empty(), "nothing was staged");

	// A trailing slash on an actual directory still works.
	wt.add(&["sub/"], "").await.unwrap();
	assert!(ls_files(w).iter().any(|l| l.ends_with("\tsub/x.txt")));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_rewrites_index_on_file_directory_type_change() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("add-typechange");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let wt = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir);

	// Stage `thing` as a file.
	std::fs::write(work.join("thing"), b"FILE\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1);
	assert!(files[0].ends_with("\tthing"));

	// Replace it with a directory and re-add: the stale file entry must be dropped.
	std::fs::remove_file(work.join("thing")).unwrap();
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child.txt"), b"CHILD\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1, "no stale `thing` file entry remains");
	assert!(files[0].ends_with("\tthing/child.txt"));

	// And the reverse: directory back to a file drops the `thing/child.txt` entry.
	std::fs::remove_dir_all(work.join("thing")).unwrap();
	std::fs::write(work.join("thing"), b"FILE2\n").unwrap();
	wt.add(&["."], "").await.unwrap();
	let files = ls_files(w);
	assert_eq!(files.len(), 1);
	assert!(files[0].ends_with("\tthing"));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_stages_deletions_under_a_directory_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("add-del");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("keep.txt"), b"keep\n").unwrap();
	std::fs::write(work.join("gone.txt"), b"gone\n").unwrap();
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/leaf.txt"), b"leaf\n").unwrap();

	// Stage everything, then delete a top-level and a nested tracked file and re-run `add .`.
	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	open().add(&["."], "").await.unwrap();
	std::fs::remove_file(work.join("gone.txt")).unwrap();
	std::fs::remove_file(work.join("dir/leaf.txt")).unwrap();
	open().add(&["."], "").await.unwrap();
	let ours = ls_files(w);

	// git, staging the same on-disk state from an empty index, records exactly the survivor.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", w, "add", "."]);
	let theirs = ls_files(w);

	assert_eq!(ours, theirs, "`add .` must stage deletions like git");
	assert_eq!(ours.len(), 1, "only keep.txt survives: {ours:?}");
	assert!(ours[0].ends_with("\tkeep.txt"));

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn add_directory_pathspec_stages_a_removed_subtree_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("add-rmdir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("top.txt"), b"top\n").unwrap();
	std::fs::create_dir_all(work.join("pkg")).unwrap();
	std::fs::write(work.join("pkg/a.rs"), b"a\n").unwrap();
	std::fs::write(work.join("pkg/b.rs"), b"b\n").unwrap();

	let open = || {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
	};
	// Stage everything, then remove the whole `pkg` directory and `add pkg` (a directory pathspec
	// that no longer resolves on disk): its tracked children must be staged as deletions.
	open().add(&["."], "").await.unwrap();
	std::fs::remove_dir_all(work.join("pkg")).unwrap();
	open().add(&["pkg"], "").await.unwrap();
	let ours = ls_files(w);

	// git, staging the same on-disk state (only `top.txt` remains) from an empty index.
	std::fs::remove_file(git_dir.join("index")).unwrap();
	git(&["-C", w, "add", "."]);
	let theirs = ls_files(w);

	assert_eq!(
		ours, theirs,
		"`add pkg` must stage the removed subtree like git"
	);
	assert!(
		!ours.iter().any(|line| line.contains("\tpkg/")),
		"no pkg/* entries may remain: {ours:?}"
	);
	assert_eq!(ours.len(), 1);
	assert!(ours[0].ends_with("\ttop.txt"));

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
		let probe = unique_tmp("probe-add");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
