//! Safe, force-free removal, oracle-checked against stock `git worktree remove`/`list` and by having stock
//! `git` operate before/after, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{
	BranchName, DestinationKind, IdentityConflict, ProtectionReason, RemoveError, RemoveOutcome,
	RemovePolicy, RemoveRequest, WorktreeClassification, WorktreeQuery, classify, inspect, remove,
};

fn rreq(work: &std::path::Path, dest: &std::path::Path, branch: Option<&str>) -> RemoveRequest {
	RemoveRequest {
		repo: rid_at(work),
		destination: dest.to_path_buf(),
		expected_branch: branch.map(BranchName::new),
		policy: RemovePolicy::Conservative,
	}
}

/// Build a linked worktree the *oracle* way — stock `git worktree add [<pre>] <path> [<post>]` — so removal
/// is exercised against a git-authored admin layout. `pre` are flags before the path (e.g. `-b <name>`,
/// `--detach`); `post` is the optional commit-ish after it.
fn git_add_worktree(work: &std::path::Path, wt: &std::path::Path, pre: &[&str], post: &[&str]) {
	let w = work.to_str().unwrap();
	let wts = wt.to_str().unwrap();
	let mut a = vec!["-C", w, "worktree", "add"];
	a.extend_from_slice(pre);
	a.push(wts);
	a.extend_from_slice(post);
	git(&a);
}

/// A read of the destination's classification, for asserting the partial-state vocabulary. `with_status`
/// requests the status + residual scan (so a dirty/residual worktree classifies as it would for removal).
async fn classify_at(
	work: &std::path::Path,
	dest: &std::path::Path,
	with_status: bool,
) -> WorktreeClassification {
	let q = WorktreeQuery {
		repo: rid_at(work),
		destination: dest.to_path_buf(),
		expected_branch: None,
		start: None,
		with_status,
	};
	classify(&inspect(&q).await.unwrap())
}

/// A read of the destination's classification (no status), for asserting the partial-state vocabulary.
async fn classify_no_status(
	work: &std::path::Path,
	dest: &std::path::Path,
) -> WorktreeClassification {
	classify_at(work, dest, false).await
}

// ---------------------------------------------------------------------------
// safe removal of a clean, exact worktree — branch + commits retained
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removes_a_worktree_addressed_by_a_dot_segment_alias() {
	// A destination alias ending in dot-segments (`.../wt/sub/..`) identifies the checkout canonically to
	// inspection, but the destructive primitives must act on the *real* path: `remove_dir_all(".../wt/sub/..")`
	// would empty `wt` yet leave the directory, and `path_absent` on the now-dangling alias would falsely report
	// it gone — a false `Removed` over a directory that still exists. Removal resolves the destination first, so
	// the checkout is actually deleted.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-dotdot-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		// A *tracked* subdirectory so the `..` alias traverses a real path and the checkout stays clean.
		std::fs::create_dir_all(work.join("sub")).unwrap();
		std::fs::write(work.join("sub/f.txt"), "1\n").unwrap();
		let w = work.to_str().unwrap();
		git(&["-C", w, "add", "sub/f.txt"]);
		git(&["-C", w, "commit", "-q", "-m", "init"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let alias = wt.join("sub").join(".."); // .../wt/sub/.. — canonically .../wt

		let out = remove(&rreq(&work, &alias, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed via a dot-segment alias, got {out:?}"
		);
		// The real checkout directory is actually gone (not merely emptied), and git sees a consistent state.
		assert!(
			!wt.exists(),
			"{fmt}: the real checkout dir must be deleted, not left behind"
		);
		assert!(
			!git(&["-C", w, "worktree", "list", "--porcelain"]).contains("refs/heads/feature"),
			"{fmt}: git should no longer list the worktree"
		);
		assert!(git(&["-C", w, "worktree", "prune", "-n"]).is_empty());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_clean_worktree_and_retains_its_branch() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-clean-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// git agrees this is a listed worktree before we act.
		assert!(
			git(&["-C", w, "worktree", "list", "--porcelain"]).contains("branch refs/heads/feature")
		);

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		match out {
			RemoveOutcome::Removed {
				destination,
				retained_branch,
			} => {
				assert_eq!(destination, wt);
				assert_eq!(retained_branch.as_deref(), Some("refs/heads/feature"));
			}
			other => panic!("{fmt}: expected Removed, got {other:?}"),
		}

		// The checkout and the admin entry are gone; the branch and its commit are retained.
		assert!(!wt.exists(), "{fmt}: checkout dir should be gone");
		assert!(
			!git(&["-C", w, "worktree", "list", "--porcelain"]).contains("refs/heads/feature"),
			"{fmt}: git should no longer list the worktree"
		);
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), head);
		// git sees a consistent state (prune is a no-op — nothing stale left behind).
		assert!(git(&["-C", w, "worktree", "prune", "-n"]).is_empty());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn preserves_a_worktree_holding_ignored_files() {
	// Conservative preserve-mode: a worktree whose working tree holds residual git-*ignored* content (build
	// artifacts) is **refused and preserved**, even though it is "clean" in git's status vocabulary and stock
	// `git worktree remove` would delete it. gitana's ignore matcher is not fully git-faithful, so the safe
	// surface never authorises deleting a non-tracked file (untracked or ignored) — it removes only worktrees
	// containing solely tracked files. The caller clears residual content first.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-ignored-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		std::fs::write(work.join(".gitignore"), "*.log\nbuild/\n").unwrap();
		let w = work.to_str().unwrap();
		git(&["-C", w, "add", ".gitignore"]);
		git(&["-C", w, "commit", "-q", "-m", "init"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("run.log"), "noise\n").unwrap();
		std::fs::create_dir(wt.join("build")).unwrap();
		std::fs::write(wt.join("build/out.o"), "obj\n").unwrap();
		// git considers the worktree clean (ignored files omitted) — but we still preserve it.
		assert!(git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).is_empty());

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::ResidualContent { paths },
		}) = &err
		else {
			panic!("{fmt}: expected a ResidualContent refusal, got {err:?}");
		};
		// The refusal names the residual (ignored) paths so the caller knows what to clear.
		assert!(
			paths.iter().any(|p| p == "run.log"),
			"{fmt}: residual paths should include run.log, got {paths:?}"
		);
		assert!(
			paths.iter().any(|p| p == "build/out.o"),
			"{fmt}: residual paths should include build/out.o, got {paths:?}"
		);
		assert!(
			wt.join("run.log").exists(),
			"{fmt}: ignored content preserved"
		);
		assert!(
			wt.join("build/out.o").exists(),
			"{fmt}: ignored content preserved"
		);

		// `classify(inspect(with_status))` agrees with the removal decision — it also reports ResidualContent,
		// not a `Complete*` reading (which would disagree with what `remove` does).
		assert!(
			matches!(
				classify_at(&work, &wt, true).await,
				WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::ResidualContent { .. }
				}
			),
			"{fmt}: classify should agree with removal (ResidualContent)"
		);

		// After the caller clears the residual content, removal proceeds (only tracked files remain).
		std::fs::remove_file(wt.join("run.log")).unwrap();
		std::fs::remove_dir_all(wt.join("build")).unwrap();
		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed once pristine, got {out:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_non_component_double_star_ignore_does_not_hide_an_untracked_file() {
	// Regression: gitana-worktree's ignore matcher treated any `**` as recursive, so `a/**b` wrongly ignored
	// `a/a/b` (stock git reports it untracked). That false-clean would have let removal delete the untracked
	// file. The matcher-independent residual scan catches it regardless, so removal is refused (residual
	// content) and the file preserved.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-globstar-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		std::fs::write(work.join(".gitignore"), "a/**b\n").unwrap();
		let w = work.to_str().unwrap();
		std::fs::create_dir_all(work.join("a/a")).unwrap();
		std::fs::write(work.join("a/a/keep"), "k\n").unwrap();
		git(&["-C", w, "add", ".gitignore", "a/a/keep"]);
		git(&["-C", w, "commit", "-q", "-m", "init"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("a/a/b"), "untracked\n").unwrap();
		// Oracle: git sees the untracked file and refuses to remove without --force.
		assert!(git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).contains("?? a/a/b"));
		assert!(!git_ok(&[
			"-C",
			w,
			"worktree",
			"remove",
			wt.to_str().unwrap()
		]));

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::ResidualContent { paths },
		}) = &err
		else {
			panic!(
				"{fmt}: expected ResidualContent refusal (untracked file must not be hidden), got {err:?}"
			);
		};
		assert!(
			paths.iter().any(|p| p == "a/a/b"),
			"{fmt}: residual paths should include a/a/b, got {paths:?}"
		);
		assert!(wt.join("a/a/b").exists(), "{fmt}: untracked file preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_clean_worktree_under_core_filemode_false() {
	// End-to-end: with core.fileMode=false an exec-bit-only change is git-clean, so removal now proceeds
	// (previously gitana-worktree ignored the config and reported it modified → a false Dirty over-refusal).
	use std::os::unix::fs::PermissionsExt;
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-filemode-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "config", "core.fileMode", "false"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::set_permissions(wt.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
		// Sanity: git considers the worktree clean under fileMode=false.
		assert!(git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).is_empty());

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed under fileMode=false, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_linked_worktree_using_a_split_index() {
	// A linked worktree with core.splitIndex stores sharedindex.<oid> in its own admin dir. Removal must
	// route that per-worktree file to the admin (not the common dir) to merge + read the index — otherwise it
	// failed with "missing shared index" for an otherwise clean worktree.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-split-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "update-index", "--split-index"]);
		// A change committed within the worktree, so its state is clean but its index is split.
		std::fs::write(wt.join("a.txt"), b"2\n").unwrap();
		git(&["-C", wts, "add", "a.txt"]);
		git(&["-C", wts, "commit", "-q", "-m", "change"]);
		assert!(
			git(&["-C", wts, "status", "--porcelain"]).is_empty(),
			"{fmt}: sanity clean"
		);

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed for a split-index linked worktree, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_worktree_config_filemode_override_refuses_a_dirty_checkout() {
	// Data-preservation regression: with extensions.worktreeConfig, a linked worktree's config.worktree can
	// override core.fileMode. If the common config says false but the worktree override says true, an exec-bit
	// change IS a modification (git reports it dirty) — removal must read the worktree-effective config and
	// refuse, never trust the common `false` and recursively delete a genuinely-modified checkout.
	use std::os::unix::fs::PermissionsExt;
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-wtconfig-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
		git(&["-C", w, "config", "core.fileMode", "false"]); // common: ignore exec bit
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "config", "--worktree", "core.fileMode", "true"]); // override: honour it
		std::fs::set_permissions(wt.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
		// Sanity: git honours the worktree override and reports the exec-bit change as modified.
		assert!(
			git(&["-C", wts, "status", "--porcelain"]).contains("a.txt"),
			"{fmt}: git should see the exec-bit change as modified under the worktree override"
		);

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Dirty(_)
				})
			),
			"{fmt}: worktree-config fileMode override must refuse the dirty checkout, got {err:?}"
		);
		assert!(
			wt.join("a.txt").exists(),
			"{fmt}: the modified checkout is preserved"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_worktree_with_a_skip_worktree_entry() {
	// End-to-end: a sparse (skip-worktree) entry whose file is omitted is git-clean, so removal proceeds
	// (previously the omitted path read as a deletion → a false Dirty over-refusal).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-sparse-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		commit_file(&work, "b.txt", "2\n", "add b");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "update-index", "--skip-worktree", "b.txt"]);
		std::fs::remove_file(wt.join("b.txt")).unwrap();
		// Sanity: git ignores the omitted skip-worktree file.
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed with a skip-worktree entry, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn preserves_a_present_modified_skip_worktree_file() {
	// Data-preservation regression (codex round 19): a tracked file marked `--skip-worktree` and then edited in
	// place is invisible to `git status` (git ignores the working tree for skip-worktree entries), so both the
	// tracked-changes gate and the residual scan pass it — yet removing the worktree would discard the edit.
	// Removal must detect the present, content-diverged skip-worktree file and refuse, preserving the edit.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-skipwt-modified-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "original\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "update-index", "--skip-worktree", "a.txt"]);
		std::fs::write(wt.join("a.txt"), b"precious-user-edit\n").unwrap();
		// Sanity: git's own status hides the skip-worktree edit (the whole reason it is a removal hazard).
		assert!(
			git(&["-C", wts, "status", "--porcelain"]).is_empty(),
			"{fmt}: git status hides a skip-worktree edit"
		);

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::ModifiedTrackedContent { .. }
				})
			),
			"{fmt}: a present, modified skip-worktree file must refuse removal, got {err:?}"
		);
		assert_eq!(
			std::fs::read_to_string(wt.join("a.txt")).unwrap(),
			"precious-user-edit\n",
			"{fmt}: the edit is preserved intact"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_worktree_with_a_present_unmodified_skip_worktree_file() {
	// The present-skip-worktree gate must fire only on *diverged* content: a present file identical to the index
	// is reconstructable from the object store, so removal still proceeds (the gate must not over-refuse).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-skipwt-clean-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "original\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "update-index", "--skip-worktree", "a.txt"]); // present, unchanged
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: a present, unmodified skip-worktree file must not block removal, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_sparse_index_worktree_honestly() {
	// A `git sparse-checkout --sparse-index` checkout collapses out-of-cone directories into single 040000
	// sparse-directory entries that gitana does not expand. Rather than acting on the spurious add/delete pairs
	// status would report for such an index (a misleading Dirty), removal refuses with a clear
	// SparseIndexUnsupported signal and preserves the checkout. Expanding sparse indexes is a deferred follow-up.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-sparse-index-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		std::fs::create_dir_all(work.join("in")).unwrap();
		std::fs::create_dir_all(work.join("out")).unwrap();
		std::fs::write(work.join("in/f.txt"), b"1\n").unwrap();
		std::fs::write(work.join("out/f.txt"), b"2\n").unwrap();
		let w = work.to_str().unwrap();
		git(&["-C", w, "add", "."]);
		git(&["-C", w, "commit", "-q", "-m", "init"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		git(&[
			"-C",
			wts,
			"sparse-checkout",
			"init",
			"--cone",
			"--sparse-index",
		]);
		git(&["-C", wts, "sparse-checkout", "set", "in"]);
		// Sanity: the cone sparse-index checkout is clean to git, with `out/` collapsed to a sparse-dir entry.
		assert!(
			git(&["-C", wts, "status", "--porcelain"]).is_empty(),
			"{fmt}: a cone sparse-index checkout is clean to git"
		);

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::SparseIndexUnsupported
				})
			),
			"{fmt}: a sparse-index worktree must refuse honestly, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_detached_worktree() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-detached-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["--detach"], &[&head]);

		let out = remove(&rreq(&work, &wt, None)).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: expected Removed w/ no branch, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_detached_worktree_with_unreachable_commits() {
	// Commit-preservation regression (codex round 24): a detached worktree that advanced to a commit no shared
	// ref reaches would be orphaned by removal — its admin dir (dropped on removal) holds the only reference.
	// Removal refuses (naming the commit) so the caller can branch/tag it first; once anchored, it removes.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-detached-orphan-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["--detach"], &[&head1]);
		let wts = wt.to_str().unwrap();
		// Advance the *detached* HEAD to a new commit — no branch references it.
		std::fs::write(wt.join("a.txt"), b"2\n").unwrap();
		git(&["-C", wts, "add", "a.txt"]);
		git(&[
			"-C",
			wts,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"detached work",
		]);
		let head2 = git(&["-C", wts, "rev-parse", "HEAD"]).trim().to_owned();
		assert_ne!(head1, head2, "{fmt}: the detached HEAD advanced");
		assert!(
			git(&["-C", wts, "status", "--porcelain"]).is_empty(),
			"{fmt}: sanity clean"
		);

		let err = remove(&rreq(&work, &wt, None)).await.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit { commit }
				}) if commit.to_hex() == head2
			),
			"{fmt}: a detached worktree with unreachable commits must refuse naming the commit, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");

		// Once the caller anchors the commit with a branch, its commit is reachable → removal proceeds.
		git(&["-C", work.to_str().unwrap(), "branch", "saved", &head2]);
		let out = remove(&rreq(&work, &wt, None)).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: removable once the commit is anchored by a ref, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_worktree_with_a_per_worktree_ref_anchoring_an_unreachable_commit() {
	// Commit-preservation regression (codex round 30): removing a worktree deletes its whole admin dir,
	// including the per-worktree ref namespaces (`refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*`). A
	// *clean, branch-attached* checkout whose HEAD is safely on `refs/heads/*` can still have such a ref pointing
	// at an otherwise-unreachable commit; the HEAD-only reachability check missed it and removal orphaned it.
	// Every admin-local anchor must be checked, so this refuses (naming the commit).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-per-worktree-anchor-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]); // clean, HEAD on refs/heads/feature = head1

		// A dangling commit reachable from no shared ref, anchored only by a per-worktree ref in the admin.
		let tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
			.trim()
			.to_owned();
		let head2 = git(&[
			"-C",
			w,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit-tree",
			&tree,
			"-p",
			&head1,
			"-m",
			"orphan",
		])
		.trim()
		.to_owned();
		assert_ne!(head1, head2);
		let admin = work.join(".git").join("worktrees").join("wt");
		std::fs::create_dir_all(admin.join("refs").join("worktree")).unwrap();
		std::fs::write(
			admin.join("refs").join("worktree").join("save"),
			format!("{head2}\n"),
		)
		.unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit { commit }
				}) if commit.to_hex() == head2
			),
			"{fmt}: a per-worktree ref anchoring an unreachable commit must refuse, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");

		// Once that commit is anchored by a shared branch, removal proceeds.
		git(&["-C", w, "branch", "saved", &head2]);
		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: removable once the per-worktree-ref commit is anchored by a shared ref, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_worktree_with_a_symbolic_per_worktree_ref_anchor() {
	// Commit-preservation regression (codex round 31): a *symbolic* per-worktree ref
	// (`refs/worktree/save -> ORIG_HEAD`) is skipped by `RefStore::list`, so the anchor check missed the commit
	// it holds and removal orphaned it. The per-worktree side must resolve symbolic refs too (as the shared side
	// already does), so this refuses.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-sym-per-worktree-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);

		let tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
			.trim()
			.to_owned();
		let head2 = git(&[
			"-C",
			w,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit-tree",
			&tree,
			"-p",
			&head1,
			"-m",
			"orphan",
		])
		.trim()
		.to_owned();
		let admin = work.join(".git").join("worktrees").join("wt");
		std::fs::create_dir_all(admin.join("refs").join("worktree")).unwrap();
		// A per-worktree pseudoref holds the commit; the per-worktree ref is *symbolic* to it.
		std::fs::write(admin.join("ORIG_HEAD"), format!("{head2}\n")).unwrap();
		std::fs::write(
			admin.join("refs").join("worktree").join("save"),
			"ref: ORIG_HEAD\n",
		)
		.unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit { commit }
				}) if commit.to_hex() == head2
			),
			"{fmt}: a symbolic per-worktree ref anchoring an unreachable commit must refuse, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_detached_worktree_anchored_by_a_symbolic_ref() {
	// Reachability regression (codex round 28): a detached commit whose only shared anchor is a *symbolic* ref
	// under `refs/` (`refs/tags/anchor -> CUSTOM1`) is reachable — that tag survives removal — but `list("refs/")`
	// skips symbolic refs, so it was reported unreachable and the clean removal spuriously refused. Reachability
	// must also resolve symbolic refs (`symbolic_ref_targets`), so this removes.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-symbolic-anchor-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["--detach"], &[&head1]);
		let wts = wt.to_str().unwrap();
		std::fs::write(wt.join("a.txt"), b"2\n").unwrap();
		git(&["-C", wts, "add", "a.txt"]);
		git(&[
			"-C",
			wts,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"detached work",
		]);
		let head2 = git(&["-C", wts, "rev-parse", "HEAD"]).trim().to_owned();

		// Anchor head2 only by a *symbolic* shared ref: a shared `CUSTOM1` (digit → not a pseudoref → common)
		// holds the commit, and `refs/tags/anchor` is a symbolic ref to it. Both survive removal.
		let common = work.join(".git");
		std::fs::write(common.join("CUSTOM1"), format!("{head2}\n")).unwrap();
		std::fs::write(
			common.join("refs").join("tags").join("anchor"),
			"ref: CUSTOM1\n",
		)
		.unwrap();

		let out = remove(&rreq(&work, &wt, None)).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: a commit reachable via a symbolic shared ref must be removable, got {out:?}"
		);
		assert!(!wt.exists());
		// The symbolic anchor (and its terminal) survive, so the commit is not orphaned.
		assert_eq!(
			git(&[
				"-C",
				work.to_str().unwrap(),
				"rev-parse",
				"refs/tags/anchor"
			])
			.trim(),
			head2
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_cleaning_a_partial_whose_detached_head_is_unreachable() {
	// Commit-preservation regression (codex round 25): a *checkout-missing* partial still holds the admin's
	// HEAD. Cleaning it (dropping the admin) would orphan a detached commit reachable from no shared ref, so the
	// partial cleanup must refuse — not silently prune. Once anchored, it cleans.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-orphan-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["--detach"], &[&head1]);
		let wts = wt.to_str().unwrap();
		std::fs::write(wt.join("a.txt"), b"2\n").unwrap();
		git(&["-C", wts, "add", "a.txt"]);
		git(&[
			"-C",
			wts,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"detached work",
		]);
		let head2 = git(&["-C", wts, "rev-parse", "HEAD"]).trim().to_owned();
		// Simulate an interrupted operation: the checkout dir is gone, but the admin registration (with HEAD)
		// remains — a `PresentCheckoutMissing` partial the cleanup would otherwise prune.
		std::fs::remove_dir_all(&wt).unwrap();
		assert!(!wt.exists());

		let err = remove(&rreq(&work, &wt, None)).await.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit { commit }
				}) if commit.to_hex() == head2
			),
			"{fmt}: cleaning a partial with an unreachable detached HEAD must refuse, got {err:?}"
		);
		// The registration is preserved, so the commit is still anchored by the admin HEAD.
		assert!(git(&["-C", work.to_str().unwrap(), "cat-file", "-e", &head2]).is_empty());

		git(&["-C", work.to_str().unwrap(), "branch", "saved", &head2]);
		let out = remove(&rreq(&work, &wt, None)).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: the partial cleans once the commit is anchored, got {out:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_worktree_whose_head_is_symbolic_to_a_per_worktree_ref() {
	// Commit-preservation regression (codex round 25): a HEAD *symbolic* to a per-worktree ref
	// (`refs/worktree/*`, which lives in the admin dir) is not detached, but that ref does not survive removal —
	// so an otherwise-unreachable commit it anchors would be orphaned. The reachability guard must treat any
	// non-`refs/heads/*` symbolic target as needing a shared-ref walk, not as a surviving anchor.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-perworktree-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head1 = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		// Advance `feature` to a new commit, then rewind the branch so that commit is anchored by no shared ref.
		std::fs::write(wt.join("a.txt"), b"2\n").unwrap();
		git(&["-C", wts, "add", "a.txt"]);
		git(&[
			"-C",
			wts,
			"-c",
			"user.name=t",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"wt work",
		]);
		let head2 = git(&["-C", wts, "rev-parse", "HEAD"]).trim().to_owned();

		// Anchor head2 only by a *per-worktree* ref and point HEAD symbolically at it — the state removal must
		// treat as still-anchoring (so it is not orphaned by dropping the admin). Do this *before* rewinding
		// `feature`, so the branch is no longer "used by" the worktree.
		let admin = work.join(".git").join("worktrees").join("wt");
		std::fs::create_dir_all(admin.join("refs").join("worktree")).unwrap();
		std::fs::write(
			admin.join("refs").join("worktree").join("keep"),
			format!("{head2}\n"),
		)
		.unwrap();
		std::fs::write(admin.join("HEAD"), "ref: refs/worktree/keep\n").unwrap();
		// Now rewind the shared branch so head2 is anchored by no shared ref, only the per-worktree ref.
		git(&[
			"-C",
			work.to_str().unwrap(),
			"branch",
			"-f",
			"feature",
			&head1,
		]);
		// Sanity: git resolves HEAD to head2 and the worktree is clean against it.
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head2);
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());

		let err = remove(&rreq(&work, &wt, None)).await.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::UnreachableAnchoredCommit { commit }
				}) if commit.to_hex() == head2
			),
			"{fmt}: a per-worktree-symbolic HEAD at an unreachable commit must refuse, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_cleaning_a_partial_with_staged_index_changes() {
	// Safe-removal regression (codex round 26): a checkout-missing partial still holds its per-worktree index.
	// If that index has staged (uncommitted) work, cleaning the partial would erase it and orphan the staged
	// blob. Removal must refuse — as a live checkout's staged changes are a `Dirty` refusal.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-staged-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let wts = wt.to_str().unwrap();
		// Stage a new file but do not commit — it lives only in the index (and as a loose blob).
		std::fs::write(wt.join("b.txt"), b"staged\n").unwrap();
		git(&["-C", wts, "add", "b.txt"]);
		let staged_blob = git(&["-C", wts, "rev-parse", ":b.txt"]).trim().to_owned();
		// The checkout directory vanishes (external tool / crash), leaving the admin registration + its index.
		std::fs::remove_dir_all(&wt).unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::StagedContentInMissingCheckout
				})
			),
			"{fmt}: a partial with staged index changes must refuse, got {err:?}"
		);
		// Nothing was deleted: the admin registration remains, so the staged blob is still referenced.
		assert!(
			work.join(".git").join("worktrees").join("wt").exists(),
			"{fmt}: the admin registration is preserved"
		);
		assert_eq!(
			git(&["-C", work.to_str().unwrap(), "cat-file", "-p", &staged_blob]).trim(),
			"staged",
			"{fmt}: the staged blob is preserved, not orphaned"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn cleans_a_partial_whose_index_was_never_materialised() {
	// Regression (codex round 28): a `create` interrupted after `HEAD` is published but before the checkout's
	// `index` is written leaves a recoverable partial with *no* index — which is not staged work. The staged
	// check must treat an **absent** index as "nothing staged" (not conflate it with an empty index vs a
	// non-empty HEAD, a spurious all-paths staged deletion), so the partial cleans instead of refusing forever.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-noindex-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Simulate the interrupted-create boundary: checkout gone, admin HEAD present, index never materialised.
		std::fs::remove_dir_all(&wt).unwrap();
		let admin = work.join(".git").join("worktrees").join("wt");
		std::fs::remove_file(admin.join("index")).unwrap();

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: a recoverable partial with no index must clean, not refuse, got {out:?}"
		);
		assert!(!admin.exists(), "{fmt}: the admin registration is cleaned");
		// The branch (and its commit) survives the cleanup.
		assert!(!git(&["-C", work.to_str().unwrap(), "rev-parse", "feature"]).is_empty());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_trailing_slash_symlink_destination() {
	// Data-loss regression (codex round 28): a destination that is a symlink spelled with a trailing separator
	// (`.../wt-link/`) makes a naive stat *follow* it, so it could be classified as its target directory and a
	// canonical delete would then destroy the real worktree, leaving a dangling link. Removal must classify the
	// leaf symlink as a non-worktree and refuse, preserving the target.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-symlink-slash-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let link = base.join("wt-link");
		std::os::unix::fs::symlink(&wt, &link).unwrap();
		// The destination is the symlink spelled with a trailing separator.
		let with_slash = std::path::PathBuf::from(format!("{}/", link.to_str().unwrap()));

		let err = remove(&rreq(&work, &with_slash, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				&err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict {
					kind: DestinationKind::OtherFsObject
				})
			),
			"{fmt}: a trailing-slash symlink destination must refuse as OtherFsObject, got {err:?}"
		);
		// The real worktree (the symlink's target) is untouched.
		assert!(wt.exists(), "{fmt}: the real worktree is preserved");
		assert!(
			wt.join("a.txt").exists(),
			"{fmt}: the target's content is preserved"
		);
		assert!(
			!git(&[
				"-C",
				work.to_str().unwrap(),
				"worktree",
				"list",
				"--porcelain"
			])
			.contains("prunable"),
			"{fmt}: git still sees a healthy worktree"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_clean_worktree_with_a_case_variant_git_pointer() {
	// Residual-scan regression (codex round 28): on a **case-insensitive** filesystem a checkout's pointer may
	// be stored `.GIT`; `destination.join(".git")` and git resolve it to the same file, but the residual scan's
	// byte-exact `.git` skip misreported `.GIT` as untracked content and blocked removal. The scan now skips the
	// root gitfile by filesystem identity, so this clean worktree removes. (Gated: on a case-sensitive
	// filesystem `.GIT` is a genuinely distinct file and this scenario does not arise.)
	let probe = unique_tmp("case-probe");
	std::fs::write(probe.join("x"), b"").unwrap();
	let case_insensitive = probe.join("X").exists();
	let _ = std::fs::remove_dir_all(&probe);
	if !case_insensitive {
		eprintln!("skipping: case-sensitive filesystem");
		return;
	}

	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-case-git-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Store the checkout's gitfile under a case variant — the same file on this filesystem, but its dir entry
		// now reads `.GIT`.
		std::fs::rename(wt.join(".git"), wt.join(".GIT")).unwrap();
		assert!(
			git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).is_empty(),
			"{fmt}: sanity — git resolves the case-variant pointer and sees a clean tree"
		);

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: a case-variant `.GIT` pointer must not read as residual content, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn preserves_an_untracked_hardlink_to_the_gitfile() {
	// Preservation regression (codex round 29): the residual scan skips the root `.git` pointer by identity, but
	// an untracked *hard link* to that gitfile under another name (`ln .git gitfile-backup`) shares its inode —
	// it must NOT be skipped (and thus deleted), since removal trusts the residual scan over `status`'s untracked
	// list. Requiring a `.git`-equivalent leaf name (not just inode identity) preserves it: removal refuses.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-gitlink-backup-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// An untracked hard link to the checkout's `.git` gitfile — same inode, different (non-`.git`) name.
		std::fs::hard_link(wt.join(".git"), wt.join("gitfile-backup")).unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::ResidualContent { paths },
		}) = &err
		else {
			panic!("{fmt}: an untracked gitfile hard link must be residual, got {err:?}");
		};
		assert!(
			paths.iter().any(|p| p == "gitfile-backup"),
			"{fmt}: the hard link is reported residual, got {paths:?}"
		);
		assert!(
			wt.join("gitfile-backup").exists(),
			"{fmt}: the untracked hard link is preserved, not deleted"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removal_succeeds_despite_stale_trash_remnants() {
	// Robustness regression (codex round 29): de-registration renames the admin to
	// `<common>/.gitana-removing.<pid>.<seq>`. After a crash + PID reuse a non-empty remnant at that name would
	// make the rename fail (`ENOTEMPTY`) *after* the checkout is already deleted, stranding the registration as a
	// false `Incomplete`. De-registration must skip a pre-existing trash name. Occupy a generous block of trash
	// names for this pid so the removal's sequence collides, then confirm it still cleans.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-trash-collision-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Pre-create non-empty trash remnants across the low sequence range for this process.
		let common = work.join(".git");
		let pid = std::process::id();
		for n in 0..512u32 {
			let remnant = common.join(format!(".gitana-removing.{pid}.{n}"));
			std::fs::create_dir_all(&remnant).unwrap();
			std::fs::write(remnant.join("cruft"), b"x").unwrap(); // non-empty → rename onto it would fail
		}

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: removal must succeed despite stale trash remnants, got {out:?}"
		);
		assert!(!wt.exists());
		assert!(
			!git(&[
				"-C",
				work.to_str().unwrap(),
				"worktree",
				"list",
				"--porcelain"
			])
			.contains("refs/heads/feature"),
			"{fmt}: the registration is de-registered (not stranded Incomplete)"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removing_an_unborn_orphan_worktree_retains_no_branch() {
	// A clean orphan worktree (`worktree add --orphan -b topic`) before its first commit has an *unborn* HEAD:
	// it names `refs/heads/topic`, but no such ref exists yet. Removal must report `retained_branch: None`
	// (there is no ref to retain), not the unborn branch name.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-orphan-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		// `--orphan` is a recent git feature; skip the case where the local git lacks it.
		if !git_ok(&[
			"-C",
			w,
			"worktree",
			"add",
			"--orphan",
			"-b",
			"topic",
			wt.to_str().unwrap(),
		]) {
			eprintln!("note: skipping orphan case ({fmt}) — git lacks `worktree add --orphan`");
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}

		let out = remove(&rreq(&work, &wt, Some("topic"))).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: expected Removed with no retained branch, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removing_from_a_bare_repository() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-bare-{fmt}"));
		let origin = base.join("origin");
		init_repo(&origin, fmt);
		commit_file(&origin, "a.txt", "1\n", "init");
		let bare = base.join("bare.git");
		git(&[
			"clone",
			"--bare",
			"-q",
			origin.to_str().unwrap(),
			bare.to_str().unwrap(),
		]);
		let b = bare.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			b,
			"worktree",
			"add",
			wt.to_str().unwrap(),
			"-b",
			"topic",
		]);

		let out = remove(&RemoveRequest {
			repo: rid_bare(&bare),
			destination: wt.clone(),
			expected_branch: Some(BranchName::new("topic")),
			policy: RemovePolicy::Conservative,
		})
		.await
		.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(!wt.exists());
		assert!(!git(&["-C", b, "worktree", "list", "--porcelain"]).contains("refs/heads/topic"));
		let _ = std::fs::remove_dir_all(&base);
	}
}

// ---------------------------------------------------------------------------
// refusals — dirty, locked, primary, identity mismatch (matchable, no data loss)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refuses_a_worktree_with_untracked_files() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-untracked-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("untracked.txt"), "x\n").unwrap();

		// git refuses the same removal without --force; so must we.
		assert!(!git_ok(&[
			"-C",
			w,
			"worktree",
			"remove",
			wt.to_str().unwrap()
		]));
		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		// An untracked file is residual content (no *tracked* changes), reported with its path.
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::ResidualContent { paths },
		}) = &err
		else {
			panic!("{fmt}: expected ResidualContent refusal, got {err:?}");
		};
		assert!(
			paths.iter().any(|p| p == "untracked.txt"),
			"{fmt}: residual paths should include untracked.txt, got {paths:?}"
		);
		// Nothing was deleted — the untracked file and the worktree survive.
		assert!(wt.join("untracked.txt").exists());
		assert!(git(&["-C", w, "worktree", "list", "--porcelain"]).contains("refs/heads/feature"));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_worktree_with_modified_tracked_files() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-modified-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("a.txt"), "changed\n").unwrap();

		let err = remove(&rreq(&work, &wt, None)).await.unwrap_err();
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::Dirty(report),
		}) = err
		else {
			panic!("{fmt}: expected Dirty refusal, got {err:?}");
		};
		assert!(
			report.has_unstaged(),
			"{fmt}: report should show the unstaged change"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_conflicted_worktree() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-conflict-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "base\n", "init");
		let w = work.to_str().unwrap();
		// A branch that diverges from main on the same file, checked out in a worktree, then merged → conflict.
		git(&["-C", w, "branch", "feature"]);
		commit_file(&work, "a.txt", "main\n", "main change");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &[], &["feature"]);
		let wts = wt.to_str().unwrap();
		std::fs::write(wt.join("a.txt"), "feature\n").unwrap();
		git(&["-C", wts, "commit", "-q", "-am", "feature change"]);
		// Merge main into the worktree → conflicting index (UU).
		assert!(!git_ok(&["-C", wts, "merge", "main"]));
		assert!(git(&["-C", wts, "status", "--porcelain"]).contains("UU"));

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		let RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::Dirty(report),
		}) = err
		else {
			panic!("{fmt}: expected Dirty refusal for a conflict, got {err:?}");
		};
		assert!(
			report.has_conflicts(),
			"{fmt}: report should show the conflict"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_locked_worktree_reporting_the_reason() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-locked-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			wt.to_str().unwrap(),
		]);

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { reason: Some(ref r) }
				}) if r == "busy"
			),
			"{fmt}: expected Locked(busy) refusal, got {err:?}"
		);
		assert!(wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_locked_worktree_refuses_before_reading_a_broken_index() {
	// Lock-first: a locked worktree is refused with the structured `Locked` reason even when its index is
	// unreadable — the protection preflight does not compute status, so it never surfaces a `Failed` in place
	// of the mandatory lock refusal (matching git's lock-first order).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-lock-brokenindex-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = canonical(&work.join(".git")).join("worktrees").join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			wt.to_str().unwrap(),
		]);
		// Corrupt BOTH the per-worktree index and HEAD so any status computation *or* HEAD resolution would
		// fail — the lock must still be reported first (git's lock-first order), never a `Failed`.
		std::fs::write(admin.join("index"), b"not a valid index").unwrap();
		std::fs::write(admin.join("HEAD"), b"\x00 not a ref or oid\n").unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { reason: Some(ref r) }
				}) if r == "busy"
			),
			"{fmt}: expected Locked (not Failed) despite the broken index/HEAD, got {err:?}"
		);
		assert!(wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_the_primary_worktree() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-primary-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");

		let err = remove(&rreq(&work, &work, None)).await.unwrap_err();
		assert!(
			matches!(err, RemoveError::IsPrimaryWorktree(ref p) if p == &work),
			"{fmt}: expected IsPrimaryWorktree, got {err:?}"
		);
		assert!(work.join("a.txt").exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_an_identity_mismatch_on_the_pinned_branch() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-mismatch-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);

		// The caller expects `other`, but the worktree carries `feature` → identity mismatch, refused.
		let err = remove(&rreq(&work, &wt, Some("other"))).await.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::IdentityConflict {
					detail: IdentityConflict::RegisteredToDifferentBranch { found: Some(ref b) }
				}) if b == "refs/heads/feature"
			),
			"{fmt}: expected RegisteredToDifferentBranch, got {err:?}"
		);
		assert!(wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_non_directory_destination_is_a_destination_conflict_not_a_failure() {
	// A regular file (or other non-directory) at the destination classifies as `OtherFsObject`; removal must
	// refuse it as a structured `DestinationConflict`, never a `Failed` from stat-ing `<file>/.git` (`ENOTDIR`).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-nondir-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let dest = base.join("afile");
		std::fs::write(&dest, "i am a file\n").unwrap();

		let err = remove(&rreq(&work, &dest, None)).await.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict {
					kind: DestinationKind::OtherFsObject
				})
			),
			"{fmt}: expected DestinationConflict, got {err:?}"
		);
		assert!(dest.exists(), "{fmt}: the file is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_worktree_with_a_near_name_max_admin_name() {
	// A near-`NAME_MAX` worktree name yields a near-`NAME_MAX` admin directory name. De-registration renames
	// the admin to a **bounded** trash name (not the admin's own name), so the rename cannot fail with
	// `ENAMETOOLONG` after the checkout is deleted. Removal completes normally.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-longname-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let long = "w".repeat(240);
		let wt = base.join(&long);
		// Some filesystems reject a 240-byte component; skip if `git worktree add` can't create it.
		if !git_ok(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]) {
			eprintln!(
				"note: skipping long-name case ({fmt}) — filesystem rejects the 240-byte component"
			);
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed for a long admin name, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_unrelated_content_it_does_not_own() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-unrelated-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let dest = base.join("stuff");
		std::fs::create_dir_all(&dest).unwrap();
		std::fs::write(dest.join("mine.txt"), "keep\n").unwrap();

		let err = remove(&rreq(&work, &dest, None)).await.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict {
					kind: DestinationKind::UnrelatedContent
				})
			),
			"{fmt}: expected DestinationConflict, got {err:?}"
		);
		assert!(
			dest.join("mine.txt").exists(),
			"{fmt}: unrelated data preserved"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

// ---------------------------------------------------------------------------
// idempotence + preservation of other worktrees
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removal_is_idempotent() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-idem-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);

		assert!(matches!(
			remove(&rreq(&work, &wt, Some("feature"))).await.unwrap(),
			RemoveOutcome::Removed { .. }
		));
		// A second removal after it is gone is a no-op, not a failure — the branch is still retained.
		let again = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(again, RemoveOutcome::AlreadyAbsent { .. }),
			"{fmt}: expected AlreadyAbsent, got {again:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_absent_destination_with_a_retained_branch_is_already_absent() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-absent-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// A branch exists but no worktree was ever established at `wt` (an interrupted create's residue).
		git(&["-C", w, "branch", "feature", &head]);
		let wt = base.join("wt");

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::AlreadyAbsent { .. }),
			"{fmt}: expected AlreadyAbsent, got {out:?}"
		);
		// The branch is never deleted by a removal.
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), head);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removing_one_worktree_leaves_another_untouched() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-other-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt1 = base.join("wt1");
		let wt2 = base.join("wt2");
		git_add_worktree(&work, &wt1, &["-b", "one"], &[]);
		git_add_worktree(&work, &wt2, &["-b", "two"], &[]);

		remove(&rreq(&work, &wt1, Some("one"))).await.unwrap();

		// The other worktree's administration is intact and git still operates in it.
		assert!(wt2.exists());
		assert!(git(&["-C", w, "worktree", "list", "--porcelain"]).contains("branch refs/heads/two"));
		assert!(git(&["-C", wt2.to_str().unwrap(), "status", "--porcelain"]).is_empty());
		let _ = std::fs::remove_dir_all(&base);
	}
}

// ---------------------------------------------------------------------------
// recoverable mid-checkout partial (F2): classify recoverable + clean to unblock retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cleans_a_recoverable_partial_and_unblocks_retry() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// A recoverable partial with an **empty** destination: the checkout's `.git` gitfile and its files are
		// gone (an interrupted create before any files were written, or after a manual clean-up), leaving an
		// empty directory and the retained admin registration. There is no unknown content to lose.
		std::fs::remove_file(wt.join(".git")).unwrap();
		std::fs::remove_file(wt.join("a.txt")).unwrap();
		assert!(
			std::fs::read_dir(&wt).unwrap().next().is_none(),
			"{fmt}: dest should be empty"
		);

		// It classifies as a recoverable partial (PartialRegistered), NOT a destination conflict.
		assert!(
			matches!(
				classify_no_status(&work, &wt).await,
				WorktreeClassification::PartialRegistered { .. }
			),
			"{fmt}: expected PartialRegistered classification"
		);

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		// The admin and the leftover files are gone; the branch is retained.
		assert!(!wt.exists(), "{fmt}: leftover checkout should be cleaned");
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), head);
		assert!(git(&["-C", w, "worktree", "prune", "-n"]).is_empty());
		// A retry now sees an absent destination (unblocked), not a lingering conflict.
		let again = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(again, RemoveOutcome::AlreadyAbsent { .. }),
			"{fmt}: {again:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn removes_a_partial_whose_checkout_dir_is_gone() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-gone-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// The whole checkout directory is gone; only the admin registration remains.
		std::fs::remove_dir_all(&wt).unwrap();

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(!git(&["-C", w, "worktree", "list", "--porcelain"]).contains("refs/heads/feature"));
		let _ = std::fs::remove_dir_all(&base);
	}
}

// ---------------------------------------------------------------------------
// data-preservation guards (regressions for the removal safety review)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_registration_naming_the_primary_never_deletes_it() {
	// A malformed/stale admin whose `gitdir` records the *primary* checkout must never let removal delete the
	// primary repository: primary identity is judged from the checkout itself, ahead of any registration.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-stale-primary-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let common = canonical(&work.join(".git"));
		// Forge a linked-worktree admin that points back at the primary checkout.
		let admin = common.join("worktrees/fake");
		std::fs::create_dir_all(&admin).unwrap();
		std::fs::write(
			admin.join("gitdir"),
			format!("{}\n", work.join(".git").display()),
		)
		.unwrap();
		std::fs::write(admin.join("commondir"), "../..\n").unwrap();
		std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

		let err = remove(&rreq(&work, &work, None)).await.unwrap_err();
		assert!(
			matches!(err, RemoveError::IsPrimaryWorktree(_)),
			"{fmt}: expected IsPrimaryWorktree, got {err:?}"
		);
		// The primary checkout and its `.git` are untouched.
		assert!(
			work.join("a.txt").exists(),
			"{fmt}: primary file must survive"
		);
		assert!(
			work.join(".git").exists(),
			"{fmt}: primary .git must survive"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_partial_path_reused_for_unknown_content_is_preserved() {
	// A stale `PresentCheckoutMissing` registration whose recorded path was reused for a *non-empty* directory
	// of the user's own files must be preserved — no signal proves the current contents are this worktree's
	// own, so removal refuses (a `DestinationConflict`) rather than deleting unknown data.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-reused-partial-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// The checkout is replaced by an unrelated directory of the user's files (its `.git` gone, its tracked
		// file removed, and new untracked content added) — a retained registration over reused content.
		std::fs::remove_file(wt.join(".git")).unwrap();
		std::fs::remove_file(wt.join("a.txt")).unwrap();
		std::fs::write(wt.join("my-notes.txt"), "important\n").unwrap();

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict {
					kind: DestinationKind::UnrelatedContent
				})
			),
			"{fmt}: expected DestinationConflict, got {err:?}"
		);
		// The user's file survives.
		assert!(
			wt.join("my-notes.txt").exists(),
			"{fmt}: unknown data must be preserved"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_git_created_prunable_with_leftover_content_is_not_auto_cleaned() {
	// A worktree git created whose `.git` is later removed leaves a non-empty, unattributable directory (git's
	// own "prunable"). The safe removal must not delete its contents — it refuses and preserves both the
	// leftover files and the registration, exactly as `git worktree remove` refuses a prunable-with-directory.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-git-prunable-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::remove_file(wt.join(".git")).unwrap(); // now a git-style "prunable", no gitana marker

		let err = remove(&rreq(&work, &wt, Some("feature")))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: expected DestinationConflict, got {err:?}"
		);
		assert!(
			wt.join("a.txt").exists(),
			"{fmt}: leftover content preserved"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_partial_whose_branch_is_checked_out_elsewhere_is_a_branch_use_conflict() {
	// A retained partial whose requested branch is force-checked-out in another worktree must classify as a
	// branch-use conflict — not a "prune and retry" partial — since a create retry stays blocked by the other
	// checkout.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-branchuse-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt1 = base.join("wt1");
		let wt2 = base.join("wt2");
		git_add_worktree(&work, &wt1, &["-b", "feature"], &[]);
		// A second, forced checkout of the same branch elsewhere.
		git_add_worktree(&work, &wt2, &["--force"], &["feature"]);
		// wt1 becomes an *empty* partial (its `.git` and files removed) — so the recoverable-partial fast path
		// is reachable and the branch-use conflict must out-rank it.
		std::fs::remove_file(wt1.join(".git")).unwrap();
		std::fs::remove_file(wt1.join("a.txt")).unwrap();

		let q = WorktreeQuery {
			repo: rid_at(&work),
			destination: wt1.clone(),
			expected_branch: Some(BranchName::new("feature")),
			start: None,
			with_status: false,
		};
		let classification = classify(&inspect(&q).await.unwrap());
		assert!(
			matches!(
				classification,
				WorktreeClassification::BranchUseConflict { .. }
			),
			"{fmt}: expected BranchUseConflict, got {classification:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_undeletable_admin_child_still_deregisters_the_worktree() {
	// Even when an admin child cannot be unlinked (here, an unwritable `logs` dir — standing in for an
	// immutable file no tool can delete), removal de-registers the worktree **atomically** by renaming the
	// admin out of `worktrees/` before deleting its bytes. So the registration can never be left in a
	// half-deleted, unrecognisable state: `remove` reports `Removed`, git no longer lists the worktree, the
	// branch is retained, and a repeat is idempotent — any undeletable remnant is harmless cruft *outside*
	// `worktrees/`.
	use std::os::unix::fs::PermissionsExt;
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-undeletable-admin-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let common = canonical(&work.join(".git"));
		let admin = common.join("worktrees").join("wt");
		let logs = admin.join("logs");
		assert!(
			logs.join("HEAD").exists(),
			"{fmt}: expected a seeded HEAD reflog"
		);
		std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500)).unwrap();

		let out = remove(&rreq(&work, &wt, Some("feature"))).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: expected Removed (atomic de-register despite undeletable child), got {out:?}"
		);
		// The registration is gone — git does not list it, the admin path is gone, and the branch survives.
		assert!(
			!admin.exists(),
			"{fmt}: admin entry removed from worktrees/"
		);
		assert!(!git(&["-C", w, "worktree", "list", "--porcelain"]).contains("refs/heads/feature"));
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), head);
		// Idempotent: a repeat sees the worktree already absent.
		assert!(matches!(
			remove(&rreq(&work, &wt, Some("feature"))).await.unwrap(),
			RemoveOutcome::AlreadyAbsent { .. }
		));

		// Cleanup: restore perms on any leftover cruft so the temp dir can be removed.
		if let Ok(entries) = std::fs::read_dir(&common) {
			for entry in entries.flatten() {
				if entry
					.file_name()
					.to_string_lossy()
					.starts_with(".gitana-removing-")
				{
					let _ = std::fs::set_permissions(
						entry.path().join("logs"),
						std::fs::Permissions::from_mode(0o755),
					);
				}
			}
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_worktree_enclosing_the_common_dir_is_never_recursively_deleted() {
	// A supported relocated-bare topology where the repository's common dir lives *inside* the checkout
	// (`<dest>/meta.git`, git-ignored so the worktree is clean). Recursively deleting the checkout would
	// destroy the repository's refs and objects — including the retained branch — so removal must refuse.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-encloses-{fmt}"));
		let origin = base.join("origin");
		init_repo(&origin, fmt);
		let o = origin.to_str().unwrap();
		std::fs::write(origin.join(".gitignore"), "meta.git/\n").unwrap();
		std::fs::write(origin.join("f.txt"), "hi\n").unwrap();
		git(&["-C", o, "add", "."]);
		git(&["-C", o, "commit", "-q", "-m", "init"]);

		// Bare clone, add a worktree at `dest`, then move the bare repo *inside* `dest` and repair pointers.
		let meta_tmp = base.join("meta.git");
		git(&["clone", "--bare", "-q", o, meta_tmp.to_str().unwrap()]);
		let dest = base.join("dest");
		git(&[
			"-C",
			meta_tmp.to_str().unwrap(),
			"worktree",
			"add",
			"-q",
			dest.to_str().unwrap(),
			"-b",
			"feature",
		]);
		let common = dest.join("meta.git");
		std::fs::rename(&meta_tmp, &common).unwrap();
		let c = common.to_str().unwrap();
		git(&["-C", c, "worktree", "repair"]);
		git(&["-C", c, "worktree", "repair", dest.to_str().unwrap()]);
		// Sanity: git sees `dest` as a clean worktree (meta.git ignored).
		assert!(git(&["-C", dest.to_str().unwrap(), "status", "--porcelain"]).is_empty());

		let req = RemoveRequest {
			repo: rid_bare(&common),
			destination: dest.clone(),
			expected_branch: Some(BranchName::new("feature")),
			policy: RemovePolicy::Conservative,
		};
		let err = remove(&req).await.unwrap_err();
		assert!(
			matches!(err, RemoveError::EnclosesRepository(_)),
			"{fmt}: expected EnclosesRepository, got {err:?}"
		);
		// The repository (refs + objects) and the checkout survive.
		assert!(
			common.join("HEAD").exists(),
			"{fmt}: repository storage must survive"
		);
		assert!(dest.join("f.txt").exists(), "{fmt}: checkout must survive");
		assert_eq!(
			git(&["-C", c, "rev-parse", "feature"]).trim().len(),
			if fmt == "sha1" { 40 } else { 64 }
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

// ---------------------------------------------------------------------------
// GitCompat force policy — git's `worktree remove -f` / `-f -f`
// ---------------------------------------------------------------------------

fn force_req(
	work: &std::path::Path,
	dest: &std::path::Path,
	branch: Option<&str>,
	force: u8,
) -> RemoveRequest {
	RemoveRequest {
		repo: rid_at(work),
		destination: dest.to_path_buf(),
		expected_branch: branch.map(BranchName::new),
		policy: RemovePolicy::GitCompat { force },
	}
}

#[tokio::test]
async fn force_removes_a_dirty_worktree() {
	// `git worktree remove -f` deletes a worktree with modified tracked files, which Conservative refuses.
	// GitCompat{force:1} overrides the Dirty gate; the branch is still retained (removal never deletes a ref).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-dirty-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("a.txt"), "changed\n").unwrap();

		// Conservative refuses (as git does without --force).
		assert!(remove(&rreq(&work, &wt, Some("feature"))).await.is_err());
		// GitCompat force 1 removes it, like `git worktree remove -f`.
		let out = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap();
		assert!(matches!(out, RemoveOutcome::Removed { .. }));
		assert!(!wt.exists(), "{fmt}: the dirty worktree was removed");
		assert!(git_ok(&[
			"-C",
			w,
			"rev-parse",
			"--verify",
			"refs/heads/feature"
		]));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removes_an_untracked_worktree() {
	// A worktree with genuinely untracked (non-ignored) files: Conservative refuses (ResidualContent), a
	// single `-f` removes it (deleting the untracked file with the checkout).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-untracked-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("untracked.txt"), "x\n").unwrap();

		assert!(remove(&rreq(&work, &wt, None)).await.is_err());
		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(matches!(out, RemoveOutcome::Removed { .. }));
		assert!(!wt.exists(), "{fmt}: worktree + untracked file removed");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_locked_worktree_needs_double_force() {
	// git: a single `-f` does NOT remove a locked worktree; `-f -f` does. GitCompat{force:1} still refuses
	// Locked, force 2 removes it.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-locked-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			wt.to_str().unwrap(),
		]);

		// One `-f` still refuses the lock.
		let err = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::Locked { .. }
				})
			),
			"{fmt}: one -f must still refuse a lock, got {err:?}"
		);
		assert!(wt.exists());
		// `-f -f` removes it.
		let out = remove(&force_req(&work, &wt, Some("feature"), 2))
			.await
			.unwrap();
		assert!(matches!(out, RemoveOutcome::Removed { .. }));
		assert!(!wt.exists(), "{fmt}: -f -f removes a locked worktree");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn git_compat_force_zero_refuses_residual_but_one_force_removes() {
	// The force-0 **safety divergence**: git's plain `worktree remove` deletes a worktree whose only residue
	// is git-*ignored* content (build artifacts), but authorising that would need a git-faithful ignore
	// matcher the crate deliberately lacks — its residual scan is matcher-independent, so it never deletes a
	// non-tracked file on a possibly-wrong match. So `GitCompat { force: 0 }` still refuses residual
	// (ignored *or* untracked) content (diverging from git on the safe side); a single `-f` removes it.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force0-ignored-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		std::fs::write(work.join(".gitignore"), "*.log\nbuild/\n").unwrap();
		let w = work.to_str().unwrap();
		git(&["-C", w, "add", ".gitignore"]);
		git(&["-C", w, "commit", "-q", "-m", "init"]);
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		std::fs::write(wt.join("run.log"), "noise\n").unwrap();
		std::fs::create_dir(wt.join("build")).unwrap();
		std::fs::write(wt.join("build/out.o"), "obj\n").unwrap();
		// git considers it clean (ignored files omitted) — plain `git worktree remove` would delete it.
		assert!(git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).is_empty());

		// GitCompat force 0 refuses it (the safe divergence), naming the residual paths — it does not trust
		// the ignore matcher to authorise a delete.
		let err = remove(&force_req(&work, &wt, Some("feature"), 0))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::ProtectedWithReason {
					reason: ProtectionReason::ResidualContent { .. }
				})
			),
			"{fmt}: force 0 refuses residual content, got {err:?}"
		);
		assert!(wt.exists());

		// A single `-f` removes it, deleting the ignored residue with the checkout.
		let out = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap();
		assert!(matches!(out, RemoveOutcome::Removed { .. }));
		assert!(!wt.exists(), "{fmt}: `-f` removes an ignored-only worktree");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn double_force_still_refuses_a_branch_identity_mismatch() {
	// Force never overrides **identity**: `-f -f` on a locked worktree pinned to the WRONG `expected_branch`
	// is an IdentityConflict, not a delete. (`decide_remove` checks the lock before the identity conflict, so
	// the forced override must re-assert identity — else a mis-pinned `-f -f` would delete the wrong worktree.)
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-identity-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			wt.to_str().unwrap(),
		]);

		// The worktree is on `feature`, but the request pins `wrong`. Even `-f -f` must refuse and not delete.
		let err = remove(&force_req(&work, &wt, Some("wrong"), 2))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::IdentityConflict { .. })
			),
			"{fmt}: a mis-pinned -f -f must be an identity conflict, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the mis-pinned worktree must survive");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn double_force_refuses_a_nonempty_reused_partial_destination() {
	// A locked checkout-missing partial whose path was reused for unrelated content: even `-f -f` (overriding
	// the lock) must not delete it. The forced path validates `.git` structure — a *present* directory whose
	// checkout `.git` is gone fails that validation, so the removal refuses (a `DestinationConflict`) rather
	// than deleting the reused content.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-reused-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			wt.to_str().unwrap(),
		]);
		// Make it a checkout-missing partial (drop the checkout's `.git` + files), then reuse the path for
		// unrelated non-empty content.
		std::fs::remove_file(wt.join(".git")).unwrap();
		std::fs::remove_file(wt.join("a.txt")).unwrap();
		std::fs::write(wt.join("unrelated.txt"), "keep me\n").unwrap();

		let err = remove(&force_req(&work, &wt, Some("feature"), 2))
			.await
			.unwrap_err();
		// The overridden lock does not mask the real refusal: the reused non-empty destination surfaces as a
		// DestinationConflict, not the already-overridden lock.
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: -f -f on a non-empty reused partial must surface DestinationConflict, got {err:?}"
		);
		assert!(
			wt.join("unrelated.txt").exists(),
			"{fmt}: reused non-empty content must not be deleted"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_cleans_an_absent_partial_with_an_unreadable_index() {
	// A git-faithful forced removal never reads a checkout-missing partial's retained index (git orphans any
	// staged work at every force level), so a malformed index does not fail it: with the checkout directory
	// **absent**, a single `-f` drops the stale admin (`CleanPartial`) where a `Conservative` remove would
	// `Failed` reading that unreadable index for its staged-work check.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-badindex-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// The admin dir `<common>/worktrees/<name>`; corrupt its retained index.
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		std::fs::write(admin.join("index"), b"not a valid git index").unwrap();
		// Make it a checkout-missing partial with a genuinely **absent** destination (the whole checkout dir is
		// gone, not merely emptied) — the forced path cleans the stale admin; a *present* empty-dir partial would
		// instead refuse (see `force_refuses_a_present_empty_dir_partial`).
		std::fs::remove_dir_all(&wt).unwrap();
		assert!(!wt.exists());

		// Conservative reads the index for its staged-work check and fails on the corruption.
		assert!(
			remove(&rreq(&work, &wt, Some("feature"))).await.is_err(),
			"{fmt}: Conservative fails on the unreadable index"
		);
		// GitCompat force 1 does not read it — it cleans the stale admin, as `git worktree remove` would.
		let out = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(
			!admin.exists(),
			"{fmt}: the stale admin registration is cleaned"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_refuses_a_present_empty_dir_partial() {
	// git's forced validation is structural: a *present* directory (even an empty one) that no longer carries a
	// valid `.git` gitfile is a validation refusal, not a forced delete — "present ⇒ must contain a valid
	// `.git`". A checkout-missing partial whose directory still exists (emptied, not removed) must therefore
	// refuse under `-f`, preserving whatever now sits at the path, unlike an ABSENT partial which is cleaned.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-partial-emptydir-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Emptied but still present: drop the checkout's `.git` and its files, leaving an empty directory.
		std::fs::remove_file(wt.join(".git")).unwrap();
		std::fs::remove_file(wt.join("a.txt")).unwrap();
		assert!(wt.is_dir() && std::fs::read_dir(&wt).unwrap().next().is_none());

		// Even a second force refuses — the present (empty) directory has no valid `.git` to validate.
		let err = remove(&force_req(&work, &wt, Some("feature"), 2))
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: a present empty-dir partial must refuse under force, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the present directory is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_refuses_a_worktree_with_a_missing_admin_head() {
	// git's forced-remove HEAD gate (probed, git 2.50.1) requires HEAD to EXIST as a file: a linked worktree
	// whose `<admin>/HEAD` was deleted is rejected even under `-f -f`, not force-removed. The lean forced path
	// must refuse it too (its `admin_head_valid` gate is false when HEAD is absent).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-nohead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		std::fs::remove_file(admin.join("HEAD")).unwrap();

		// Oracle: stock git refuses even with two forces.
		assert!(
			!git_ok(&[
				"-C",
				w,
				"worktree",
				"remove",
				"--force",
				"--force",
				wt.to_str().unwrap(),
			]),
			"{fmt}: probe — git refuses a missing-admin-HEAD worktree even with --force --force"
		);
		// `None` expected-branch so the refusal is the HEAD-existence gate, not an identity mismatch.
		let out = remove(&force_req(&work, &wt, None, 2)).await;
		assert!(
			out.is_err(),
			"{fmt}: a missing admin HEAD must not be force-deleted, got {out:?}"
		);
		assert!(wt.exists(), "{fmt}: the broken checkout must survive");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removes_a_live_worktree_with_a_corrupt_index() {
	// `git worktree remove -f` skips the cleanliness scan, so a *live* checkout with a corrupt per-worktree
	// index is removed (git does the same). GitCompat force 1 does too — its status-free inspection never
	// reads the index — where a Conservative remove reads it and fails. Integrity (a readable HEAD) is still
	// validated; this checkout has one.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-badindex-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Corrupt the live checkout's per-worktree index (in the admin dir), leaving the checkout otherwise
		// valid (its `.git` gitfile and admin `HEAD` intact).
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		std::fs::write(admin.join("index"), b"not a valid git index").unwrap();

		// Conservative reads the index for its status scan and fails.
		assert!(
			remove(&rreq(&work, &wt, Some("feature"))).await.is_err(),
			"{fmt}: Conservative fails on the corrupt index"
		);
		// GitCompat force 1 removes it (status-free — never reads the index), as `git worktree remove -f` does.
		let out = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(
			!wt.exists(),
			"{fmt}: the corrupt-index worktree was force-removed"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removes_a_worktree_with_a_legacy_symlink_head() {
	// A legacy symbolic-ref HEAD — a filesystem symlink `<admin>/HEAD -> <a ref file>` — resolves to a file, so
	// git's forced-remove HEAD gate (HEAD exists && is a file; `std::fs::metadata` follows the symlink) accepts
	// it and the worktree is force-removed.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-symlinkhead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// Replace the content HEAD with a legacy symlink to a real ref file (the shared branch ref lives in the
		// common dir, so the link is absolute — `metadata` follows it to that file and sees a valid file HEAD).
		std::fs::remove_file(admin.join("HEAD")).unwrap();
		std::os::unix::fs::symlink(work.join(".git/refs/heads/feature"), admin.join("HEAD")).unwrap();

		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(
			!wt.exists(),
			"{fmt}: a worktree with a legacy symlink HEAD is force-removed"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_refuses_a_worktree_with_a_directory_admin_head() {
	// git's forced-remove HEAD gate (probed, git 2.50.1) is "HEAD exists && is a file". A `<admin>/HEAD` that is
	// a *directory* is not a file, so git refuses even under `-f -f`; the lean forced path refuses too.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-dirhead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// Replace the HEAD file with a directory.
		std::fs::remove_file(admin.join("HEAD")).unwrap();
		std::fs::create_dir(admin.join("HEAD")).unwrap();

		// `None` expected-branch so the refusal is the HEAD-existence gate, not identity.
		let out = remove(&force_req(&work, &wt, None, 2)).await;
		assert!(
			out.is_err(),
			"{fmt}: a directory admin HEAD must not be force-deleted, got {out:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removes_a_worktree_with_a_garbage_admin_head() {
	// Probe (git 2.50.1): git's forced-remove HEAD gate validates HEAD *existence*, NOT content — a present but
	// garbage `<admin>/HEAD` (neither a `ref:`, a symref, nor an object id) is still removed by
	// `git worktree remove -f -f`. The lean forced path matches: HEAD is a present file → valid-for-removal,
	// with no branch to retain (the garbage names none).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-garbagehead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// A garbage HEAD: not `ref:`, not a hex object id — but a present file. git removes it under -f -f.
		std::fs::write(admin.join("HEAD"), b"not a valid head\n").unwrap();

		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: a present garbage HEAD is removed with no branch retained, got {out:?}"
		);
		assert!(!wt.exists(), "{fmt}: the worktree was force-removed");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_cleans_an_absent_partial_with_a_missing_head() {
	// Bug-3 regression: when the destination is genuinely **absent** there is no checkout to validate, so git
	// drops the stale registration regardless of whether `<admin>/HEAD` still exists. The forced path must clean
	// such a partial (not refuse), else a missing-HEAD registration whose checkout is gone becomes uncleanable.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-absent-nohead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		std::fs::remove_file(admin.join("HEAD")).unwrap();
		std::fs::remove_dir_all(&wt).unwrap(); // genuinely absent destination
		assert!(!wt.exists());

		// `None` expected-branch — a missing HEAD carries no branch, so a pin would (correctly) be a mismatch.
		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(
			matches!(out, RemoveOutcome::Removed { .. }),
			"{fmt}: {out:?}"
		);
		assert!(
			!admin.exists(),
			"{fmt}: the stale admin registration is cleaned even with a missing HEAD"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removing_an_unborn_branch_worktree_retains_no_branch() {
	// Bug-4 regression: a worktree whose HEAD names `refs/heads/ghost` while no such ref exists (an unborn
	// branch) must report `retained_branch: None` — removal never creates the ref, and reporting a nonexistent
	// branch violates `RemoveOutcome`'s contract. The forced path confirms the shared ref exists first.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-unborn-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// A valid symbolic HEAD naming a branch that does not exist (unborn).
		std::fs::write(admin.join("HEAD"), "ref: refs/heads/ghost\n").unwrap();
		assert!(!git_ok(&[
			"-C",
			work.to_str().unwrap(),
			"rev-parse",
			"--verify",
			"refs/heads/ghost"
		]));

		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: an unborn branch must not be reported as retained, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_refuses_a_worktree_whose_admin_gitdir_points_at_a_different_file() {
	// Bug-1 regression: `admin_dirs_for` matches on the admin `gitdir` target's *parent*, so an admin whose
	// `gitdir` names a DIFFERENT filename under the destination still resolves — yet that is a cross-pointer
	// disagreement git rejects "not a working tree". The forced structural check requires BOTH directions (the
	// checkout `.git` names the admin AND the admin `gitdir` resolves to exactly `<destination>/.git`), so it
	// refuses rather than force-deleting an inconsistent worktree.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-backptr-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// Repoint the admin's `gitdir` back-pointer at a *different* filename under the destination — same parent
		// (so `admin_dirs_for` still resolves it), but no longer `<wt>/.git`. The checkout's `.git` still names
		// the admin, so only the admin->checkout direction is now wrong.
		std::fs::write(admin.join("gitdir"), format!("{}/other\n", wt.display())).unwrap();

		// `None` expected-branch so the structural (not identity) refusal is exercised.
		let err = remove(&force_req(&work, &wt, None, 2)).await.unwrap_err();
		assert!(
			matches!(
				err,
				RemoveError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: an inconsistent admin back-pointer must refuse under force, got {err:?}"
		);
		assert!(wt.exists(), "{fmt}: the checkout is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_removes_a_whitespace_padded_detached_head() {
	// Probe (git 2.50.1): git's forced-remove HEAD gate validates HEAD *existence*, not content — a space/tab-
	// padded detached `<admin>/HEAD` is still a present file, so `git worktree remove -f -f` removes it. The lean
	// forced path matches: it is valid-for-removal, and the padded id names no branch, so nothing is retained.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-paddedhead-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["--detach"], &[&head]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// A real object id, padded with spaces — a present file HEAD git still force-removes.
		std::fs::write(admin.join("HEAD"), format!("  {head}  \n")).unwrap();

		let out = remove(&force_req(&work, &wt, None, 2)).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: a whitespace-padded detached HEAD is force-removed, got {out:?}"
		);
		assert!(!wt.exists(), "{fmt}: the worktree was force-removed");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_resolves_a_symbolic_head_alias_for_identity_and_retained_branch() {
	// Bug-3 regression: identity and retained-branch reporting must resolve the HEAD symref chain to its
	// TERMINAL ref (`HEAD -> refs/heads/alias -> refs/heads/feature`), matching the conservative path — a
	// direct-only compare would both mis-refuse a `feature` pin and report `refs/heads/alias` as retained.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-alias-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// `refs/heads/alias` is a symbolic ref to `refs/heads/feature`; HEAD is symbolic to the alias.
		std::fs::write(
			work.join(".git/refs/heads/alias"),
			"ref: refs/heads/feature\n",
		)
		.unwrap();
		std::fs::write(admin.join("HEAD"), "ref: refs/heads/alias\n").unwrap();

		// Pinning `feature` must MATCH via the terminal (not mis-refuse on the direct `alias`), and the retained
		// branch is reported as the terminal `refs/heads/feature`, not the alias.
		let out = remove(&force_req(&work, &wt, Some("feature"), 1))
			.await
			.unwrap();
		assert!(
			matches!(
				&out,
				RemoveOutcome::Removed { retained_branch, .. }
					if retained_branch.as_deref() == Some("refs/heads/feature")
			),
			"{fmt}: expected Removed retaining the terminal refs/heads/feature, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_does_not_retain_a_branch_that_is_only_a_ref_namespace_directory() {
	// Bug-4 regression: `<common>/refs/heads/foo` can be a DIRECTORY (because `refs/heads/foo/bar` exists), which
	// means `foo` itself is unborn. Existence must require a loose ref FILE (or a packed-refs entry), not any
	// filesystem object, so an unborn `refs/heads/foo` is reported `retained_branch: None`.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-refdir-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		let admin = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		// Make `refs/heads/foo` a directory (a namespace) via a child ref, and point HEAD at the unborn `foo`.
		std::fs::create_dir_all(work.join(".git/refs/heads/foo")).unwrap();
		std::fs::write(work.join(".git/refs/heads/foo/bar"), format!("{head}\n")).unwrap();
		std::fs::write(admin.join("HEAD"), "ref: refs/heads/foo\n").unwrap();

		let out = remove(&force_req(&work, &wt, None, 1)).await.unwrap();
		assert!(
			matches!(
				out,
				RemoveOutcome::Removed {
					retained_branch: None,
					..
				}
			),
			"{fmt}: a ref-namespace directory must not count as an existing branch, got {out:?}"
		);
		assert!(!wt.exists());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_refuses_a_worktree_with_an_unknown_repository_extension() {
	// Probe (git 2.50.1): an unknown `extensions.*` at repositoryformatversion >= 1 makes `git worktree remove
	// -f -f` ABORT (exit 128), keeping the worktree — git will not operate on a repo format it does not fully
	// understand. The forced path validates repository format before any destructive action (requirements
	// 257-258), so it refuses too and deletes nothing.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("remove-force-ext-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git_add_worktree(&work, &wt, &["-b", "feature"], &[]);
		// Bump the format to version 1 (so extensions are read) and add an unknown extension.
		git(&["-C", w, "config", "core.repositoryformatversion", "1"]);
		git(&["-C", w, "config", "extensions.fooBar", "baz"]);

		// Oracle: stock git aborts even with two forces.
		assert!(
			!git_ok(&[
				"-C",
				w,
				"worktree",
				"remove",
				"--force",
				"--force",
				wt.to_str().unwrap(),
			]),
			"{fmt}: probe — git aborts a forced remove on an unknown repository extension"
		);

		let out = remove(&force_req(&work, &wt, Some("feature"), 2)).await;
		assert!(
			out.is_err(),
			"{fmt}: an unknown repository extension must refuse forced removal, got {out:?}"
		);
		assert!(wt.exists(), "{fmt}: the worktree is preserved");
		let _ = std::fs::remove_dir_all(&base);
	}
}
