//! Working-tree status readout, oracle-checked against `git status --porcelain=v1`, over both formats.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{RepositoryId, status};

fn sorted(porcelain: &str) -> Vec<String> {
	let mut lines: Vec<String> = porcelain.lines().map(str::to_owned).collect();
	lines.sort();
	lines
}

#[tokio::test]
async fn status_readout_distinguishes_states_and_matches_git() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "keep.txt", "k\n", "init");
		commit_file(&work, "gone.txt", "g\n", "add gone");
		commit_file(&work, "mod.txt", "m\n", "add mod");
		let wt = base.join("wt");
		let w = work.to_str().unwrap();
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		let ww = wt.to_str().unwrap();

		// Clean to start.
		let clean = status(&rid_at(&work), &wt).await.unwrap();
		assert!(clean.is_clean());
		assert_eq!(clean.destination, wt);
		assert_eq!(clean.porcelain_v1(), "");

		// Staged (a new file added), unstaged (a tracked file modified), untracked, and missing.
		std::fs::write(wt.join("staged.txt"), b"s\n").unwrap();
		git(&["-C", ww, "add", "staged.txt"]);
		std::fs::write(wt.join("mod.txt"), b"m changed\n").unwrap();
		std::fs::write(wt.join("untracked.txt"), b"u\n").unwrap();
		std::fs::remove_file(wt.join("gone.txt")).unwrap();

		let report = status(&rid_at(&work), &wt).await.unwrap();
		assert!(!report.is_clean());
		assert!(report.has_staged(), "a staged addition");
		assert!(report.has_unstaged(), "an unstaged modification");
		assert!(report.has_untracked(), "an untracked file");
		assert!(report.has_missing(), "a deleted tracked file");
		assert!(!report.has_conflicts());

		let theirs = sorted(&git(&["-C", ww, "status", "--porcelain=v1"]));
		assert_eq!(
			sorted(&report.porcelain_v1()),
			theirs,
			"{fmt}: status must match git"
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_reports_conflicts() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-conflict-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "base\n", "base");
		let w = work.to_str().unwrap();
		let def = git(&["-C", w, "rev-parse", "--abbrev-ref", "HEAD"])
			.trim()
			.to_owned();

		git(&["-C", w, "checkout", "-q", "-b", "side"]);
		std::fs::write(work.join("a.txt"), b"side\n").unwrap();
		git(&["-C", w, "commit", "-q", "-am", "side"]);
		git(&["-C", w, "checkout", "-q", &def]);
		std::fs::write(work.join("a.txt"), b"main\n").unwrap();
		git(&["-C", w, "commit", "-q", "-am", "main"]);
		let _ = git_try(&["-C", w, "merge", "side"]); // conflicts, exits non-zero

		// Status of the *main* worktree (its destination is the work-tree root).
		let report = status(&rid_at(&work), &work).await.unwrap();
		assert!(
			report.has_conflicts(),
			"{fmt}: a merge conflict must be reported"
		);
		assert!(!report.is_clean());
		let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
		assert_eq!(
			sorted(&report.porcelain_v1()),
			theirs,
			"{fmt}: conflict status must match git"
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_bare_repository_git_dir_is_an_error() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-bare-{fmt}"));
		let src = base.join("src");
		init_repo(&src, fmt);
		commit_file(&src, "a.txt", "1\n", "init");
		let bare = base.join("bare.git");
		git(&[
			"clone",
			"--bare",
			"-q",
			src.to_str().unwrap(),
			bare.to_str().unwrap(),
		]);

		// A bare repository has no working tree — a status of its git dir must be a hard error, not a
		// bogus report computed over objects/refs/config.
		assert!(status(&rid_bare(&bare), &bare).await.is_err());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_replaced_separate_git_dir_checkout_is_an_error() {
	// With `--separate-git-dir` the git directory lives *outside* the checkout, so the checkout can be
	// replaced while the git dir survives. Path equality alone would then open the replacement directory
	// with the stale index; status must instead re-check that the checkout's `.git` still names the git
	// dir and refuse when it does not.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-sgd-{fmt}"));
		let gitdir = base.join("gitdir");
		let checkout = base.join("checkout");
		git(&[
			"init",
			&format!("--object-format={fmt}"),
			"-q",
			&format!("--separate-git-dir={}", gitdir.to_str().unwrap()),
			checkout.to_str().unwrap(),
		]);
		let c = checkout.to_str().unwrap();
		git(&["-C", c, "config", "user.name", "T"]);
		git(&["-C", c, "config", "user.email", "t@e"]);
		git(&["-C", c, "config", "commit.gpgsign", "false"]);
		std::fs::write(checkout.join("a.txt"), b"1\n").unwrap();
		git(&["-C", c, "add", "a.txt"]);
		git(&["-C", c, "commit", "-q", "-m", "init"]);

		// Discovering from inside the live checkout records `worktree_root = checkout` (the path that would
		// otherwise be trusted on the next call).
		let rid = RepositoryId::discover(&checkout).await.unwrap();
		assert!(
			status(&rid, &checkout).await.unwrap().is_clean(),
			"{fmt}: the live separate-git-dir checkout statuses clean"
		);

		// Replace the checkout wholesale; the external git dir survives.
		std::fs::remove_dir_all(&checkout).unwrap();
		std::fs::create_dir_all(&checkout).unwrap();

		assert!(
			status(&rid, &checkout).await.is_err(),
			"{fmt}: a replaced separate-git-dir checkout is no longer a worktree of this repository"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_separate_git_dir_primary_from_explicit_identity() {
	// A `--separate-git-dir` primary must status correctly even when the identity is the fully-explicit
	// `at_common_dir(external_git_dir)` (no discovery, so no cached `worktree_root`). The checkout's `.git`
	// gitfile names the external git dir, which is what establishes it as the primary worktree.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-sgd-explicit-{fmt}"));
		let gitdir = base.join("gitdir");
		let checkout = base.join("checkout");
		git(&[
			"init",
			&format!("--object-format={fmt}"),
			"-q",
			&format!("--separate-git-dir={}", gitdir.to_str().unwrap()),
			checkout.to_str().unwrap(),
		]);
		let c = checkout.to_str().unwrap();
		git(&["-C", c, "config", "user.name", "T"]);
		git(&["-C", c, "config", "user.email", "t@e"]);
		git(&["-C", c, "config", "commit.gpgsign", "false"]);
		std::fs::write(checkout.join("a.txt"), b"1\n").unwrap();
		git(&["-C", c, "add", "a.txt"]);
		git(&["-C", c, "commit", "-q", "-m", "init"]);

		// Explicit identity anchored on the external git dir — no worktree_root cached.
		let rid = RepositoryId::at_common_dir(canonical(&gitdir)).unwrap();
		let report = status(&rid, &canonical(&checkout)).await.unwrap();
		assert!(
			report.is_clean(),
			"{fmt}: the separate-git-dir primary is clean"
		);

		// Make it dirty and compare to git.
		std::fs::write(checkout.join("a.txt"), b"changed\n").unwrap();
		let report = status(&rid, &canonical(&checkout)).await.unwrap();
		assert!(report.has_unstaged(), "{fmt}: an unstaged modification");
		let theirs = sorted(&git(&["-C", c, "status", "--porcelain=v1"]));
		assert_eq!(sorted(&report.porcelain_v1()), theirs, "{fmt}: matches git");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_separate_git_dir_whose_path_ends_in_a_space() {
	// A git dir whose directory name ends in a space is git-legal: the checkout's `.git` gitfile records
	// `gitdir: /d/meta \n` and git resolves `/d/meta ` *with* the space (only the newline is stripped).
	// Pointer parsing must preserve that trailing space so the primary still identifies its common dir.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-space-{fmt}"));
		let gitdir = base.join("meta "); // trailing space in the git-dir name
		let checkout = base.join("checkout");
		git(&[
			"init",
			&format!("--object-format={fmt}"),
			"-q",
			&format!("--separate-git-dir={}", gitdir.to_str().unwrap()),
			checkout.to_str().unwrap(),
		]);
		let c = checkout.to_str().unwrap();
		git(&["-C", c, "config", "user.name", "T"]);
		git(&["-C", c, "config", "user.email", "t@e"]);
		git(&["-C", c, "config", "commit.gpgsign", "false"]);
		std::fs::write(checkout.join("a.txt"), b"1\n").unwrap();
		git(&["-C", c, "add", "a.txt"]);
		git(&["-C", c, "commit", "-q", "-m", "init"]);

		let rid = RepositoryId::at_common_dir(canonical(&gitdir)).unwrap();
		let report = status(&rid, &canonical(&checkout)).await.unwrap();
		assert!(
			report.is_clean(),
			"{fmt}: a trailing-space git-dir path must still identify the primary checkout"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_separate_git_dir_whose_path_contains_a_newline() {
	// git accepts a gitfile whose admin path legitimately contains a newline (only the trailing terminator
	// is stripped) — an ancestor directory named `wi\nth` is git-legal on Unix. Pointer parsing must keep
	// the interior newline as part of the path, or the primary would not identify its common dir.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-newline-{fmt}"));
		let holder = base.join("wi\nth"); // a directory name containing a newline
		std::fs::create_dir_all(&holder).unwrap();
		let gitdir = holder.join("gd");
		let checkout = base.join("checkout");
		git(&[
			"init",
			&format!("--object-format={fmt}"),
			"-q",
			&format!("--separate-git-dir={}", gitdir.to_str().unwrap()),
			checkout.to_str().unwrap(),
		]);
		let c = checkout.to_str().unwrap();
		git(&["-C", c, "config", "user.name", "T"]);
		git(&["-C", c, "config", "user.email", "t@e"]);
		git(&["-C", c, "config", "commit.gpgsign", "false"]);
		std::fs::write(checkout.join("a.txt"), b"1\n").unwrap();
		git(&["-C", c, "add", "a.txt"]);
		git(&["-C", c, "commit", "-q", "-m", "init"]);

		let rid = RepositoryId::at_common_dir(canonical(&gitdir)).unwrap();
		let report = status(&rid, &canonical(&checkout)).await.unwrap();
		assert!(
			report.is_clean(),
			"{fmt}: a newline-containing git-dir path must still identify the primary checkout"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn status_of_a_non_worktree_path_is_an_error() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("status-err-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");

		// A path that is not a worktree of this repository — a status cannot be attributed to it.
		let stranger = base.join("stranger");
		std::fs::create_dir_all(&stranger).unwrap();
		assert!(status(&rid_at(&work), &stranger).await.is_err());

		let _ = std::fs::remove_dir_all(&base);
	}
}
