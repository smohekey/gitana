//! The `unpushed_refs` readout — the local refs (branch tips, tags, detached HEAD) whose commit no
//! remote-tracking ref contains — set up with real `git` over both object formats.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::unpushed_refs;

/// Point a remote-tracking ref at `commit`, standing in for a completed fetch of the origin.
fn set_remote(work: &std::path::Path, refname: &str, commit: &str) {
	git(&["-C", work.to_str().unwrap(), "update-ref", refname, commit]);
}

#[tokio::test]
async fn reports_only_local_commits_no_remote_ref_contains() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("unpushed-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let c1 = commit_file(&work, "a.txt", "one\n", "first");

		// Nothing is pushed yet — there is no remote-tracking ref, so `main` is unpushed.
		let none_remote = unpushed_refs(&rid_at(&work)).await.unwrap();
		let names: Vec<&str> = none_remote.iter().map(|r| r.name.as_str()).collect();
		assert_eq!(
			names,
			["refs/heads/main"],
			"{fmt}: no remote ref means nothing is pushed"
		);

		// The origin now holds `c1`: `main` is fully pushed.
		set_remote(&work, "refs/remotes/origin/main", &c1);
		assert!(
			unpushed_refs(&rid_at(&work)).await.unwrap().is_empty(),
			"{fmt}: a branch a remote ref contains is pushed",
		);

		// A new local commit the origin does not have — `main` is unpushed again, naming `c2`.
		let c2 = commit_file(&work, "a.txt", "two\n", "second");
		let after_commit = unpushed_refs(&rid_at(&work)).await.unwrap();
		assert_eq!(after_commit.len(), 1, "{fmt}: {after_commit:?}");
		assert_eq!(after_commit[0].name, "refs/heads/main");
		assert_eq!(after_commit[0].commit, c2);

		// A local tag on the unpushed commit is reported alongside the branch.
		git(&["-C", work.to_str().unwrap(), "tag", "wip"]);
		let with_tag = unpushed_refs(&rid_at(&work)).await.unwrap();
		let tag_names: Vec<&str> = with_tag.iter().map(|r| r.name.as_str()).collect();
		assert_eq!(
			tag_names,
			["refs/heads/main", "refs/tags/wip"],
			"{fmt}: an unpushed tag is reported"
		);

		// Advancing the origin to `c2` pushes both the branch and the tag's commit.
		set_remote(&work, "refs/remotes/origin/main", &c2);
		assert!(
			unpushed_refs(&rid_at(&work)).await.unwrap().is_empty(),
			"{fmt}: with the origin advanced, nothing local is unpushed",
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}
