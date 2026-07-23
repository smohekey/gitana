//! The `checkout_safety` readout — the local-only state removing a checkout would discard — set up
//! with real `git` and read back over both object formats.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::checkout_safety;

#[tokio::test]
async fn a_clean_checkout_holds_no_local_only_state() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-clean-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "keep.txt", "k\n", "init");

		let safety = checkout_safety(&rid_at(&work), &canonical(&work))
			.await
			.unwrap();

		assert_eq!(safety.destination, canonical(&work));
		assert!(!safety.has_tracked_changes);
		assert!(!safety.has_untracked);
		assert!(!safety.has_stash);
		assert!(!safety.operation_in_progress);
		assert!(safety.holds_no_local_only_state());

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn working_tree_changes_are_reported() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-dirty-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "mod.txt", "m\n", "init");
		// A tracked modification and an untracked file — the two ways a working tree loses content.
		std::fs::write(work.join("mod.txt"), b"m changed\n").unwrap();
		std::fs::write(work.join("untracked.txt"), b"u\n").unwrap();

		let safety = checkout_safety(&rid_at(&work), &canonical(&work))
			.await
			.unwrap();

		assert!(safety.has_tracked_changes, "{fmt}: a tracked modification");
		assert!(safety.has_untracked, "{fmt}: an untracked file");
		assert!(!safety.has_stash);
		assert!(!safety.operation_in_progress);
		assert!(!safety.holds_no_local_only_state());

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn ignored_files_do_not_count_as_working_changes() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-ignored-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, ".gitignore", "target/\n", "ignore target");
		std::fs::create_dir(work.join("target")).unwrap();
		std::fs::write(work.join("target").join("build.out"), b"artifact\n").unwrap();

		let safety = checkout_safety(&rid_at(&work), &canonical(&work))
			.await
			.unwrap();

		// The ignored artifact is neither a tracked change nor an untracked (non-ignored) path.
		assert!(
			!safety.has_tracked_changes,
			"{fmt}: ignored is not a tracked change"
		);
		assert!(!safety.has_untracked, "{fmt}: ignored is not untracked");
		assert!(
			safety.holds_no_local_only_state(),
			"{fmt}: ignored-only is clean"
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_stash_entry_is_reported_even_when_the_tree_is_clean() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-stash-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "mod.txt", "m\n", "init");
		let w = work.to_str().unwrap();
		// Modify a tracked file, then stash it: the working tree returns to clean, but `refs/stash` holds
		// the work.
		std::fs::write(work.join("mod.txt"), b"m changed\n").unwrap();
		git(&["-C", w, "stash"]);

		let safety = checkout_safety(&rid_at(&work), &canonical(&work))
			.await
			.unwrap();

		assert!(
			!safety.has_tracked_changes,
			"{fmt}: the tree is clean after stashing"
		);
		assert!(!safety.has_untracked);
		assert!(safety.has_stash, "{fmt}: a stash entry exists");
		assert!(
			!safety.holds_no_local_only_state(),
			"{fmt}: a stash is local-only state"
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_in_progress_merge_is_reported() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-merge-{fmt}"));
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
		let _ = git_try(&["-C", w, "merge", "side"]); // conflicts, leaving MERGE_HEAD

		let safety = checkout_safety(&rid_at(&work), &canonical(&work))
			.await
			.unwrap();

		assert!(
			safety.operation_in_progress,
			"{fmt}: a merge is in progress"
		);
		assert!(!safety.holds_no_local_only_state());

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_non_worktree_destination_is_an_error() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("safety-nonwt-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "keep.txt", "k\n", "init");
		let elsewhere = base.join("elsewhere");
		std::fs::create_dir_all(&elsewhere).unwrap();

		// A path that is not a worktree of the repository is a hard error, never a silently-clean readout.
		assert!(
			checkout_safety(&rid_at(&work), &canonical(&elsewhere))
				.await
				.is_err()
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}
