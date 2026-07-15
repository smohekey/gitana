//! Create / reconcile, oracle-checked against stock `git worktree add` and by having stock `git` operate
//! in the gitana-created worktree, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;

use common::*;
use gitana_linked_worktree::{
	BranchName, CheckoutTarget, CreateError, CreateRequest, Registration, WorktreeClassification,
	WorktreeObjectId, create,
};

fn req(work: &std::path::Path, dest: &std::path::Path, target: CheckoutTarget) -> CreateRequest {
	CreateRequest {
		repo: rid_at(work),
		destination: dest.to_path_buf(),
		target,
	}
}

fn req_bare(
	bare: &std::path::Path,
	dest: &std::path::Path,
	target: CheckoutTarget,
) -> CreateRequest {
	CreateRequest {
		repo: rid_bare(bare),
		destination: dest.to_path_buf(),
		target,
	}
}

fn new_branch(name: &str, start: WorktreeObjectId) -> CheckoutTarget {
	CheckoutTarget::NewBranch {
		name: BranchName::new(name),
		start,
	}
}

fn existing_branch(name: &str, expected_start: Option<WorktreeObjectId>) -> CheckoutTarget {
	CheckoutTarget::ExistingBranch {
		name: BranchName::new(name),
		expected_start,
	}
}

#[tokio::test]
async fn creates_a_new_branch_worktree_that_git_accepts() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-newbranch-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();

		let insp = create(&req(&work, &wt, new_branch("feature", start)), None)
			.await
			.unwrap();
		assert!(matches!(insp.registration, Registration::Present { .. }));

		let wts = wt.to_str().unwrap();
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head);
		assert_eq!(
			git(&["-C", wts, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feature"
		);
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), head);
		assert!(
			git(&["-C", w, "worktree", "list", "--porcelain"]).contains("branch refs/heads/feature")
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn checks_out_an_existing_branch() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("create-existing-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &head]);
		let wt = base.join("wt");

		create(&req(&work, &wt, existing_branch("feature", None)), None)
			.await
			.unwrap();
		let wts = wt.to_str().unwrap();
		assert_eq!(
			git(&["-C", wts, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feature"
		);
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn checking_out_a_missing_branch_is_branch_not_found() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("create-missing-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let err = create(
			&req(&work, &base.join("wt"), existing_branch("nope", None)),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(err, CreateError::BranchNotFound(ref n) if n == "nope"),
			"{fmt}: expected BranchNotFound, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn new_branch_that_already_exists_elsewhere_is_branch_exists() {
	// `NewBranch` at a start where the branch already exists at a *different* commit is refused (never
	// silently reset).
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-exists-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "init");
		let second = commit_file(&work, "b.txt", "2\n", "second");
		let w = work.to_str().unwrap();
		// `feature` exists at the *first* commit; request creates it at the *second*.
		git(&["-C", w, "branch", "feature", &first]);
		let start = WorktreeObjectId::parse(kind, &second).unwrap();
		let err = create(
			&req(&work, &base.join("wt"), new_branch("feature", start)),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(err, CreateError::BranchExists(ref n) if n == "feature"),
			"{fmt}: expected BranchExists, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn creates_a_detached_worktree() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-detached-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let target = CheckoutTarget::Detached {
			start: WorktreeObjectId::parse(kind, &head).unwrap(),
		};
		create(&req(&work, &wt, target), None).await.unwrap();
		let wts = wt.to_str().unwrap();
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head);
		assert!(
			!git_ok(&["-C", wts, "symbolic-ref", "HEAD"]),
			"{fmt}: detached"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn creates_an_orphan_worktree() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("create-orphan-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let target = CheckoutTarget::Orphan {
			name: BranchName::new("fresh"),
		};
		create(&req(&work, &wt, target), None).await.unwrap();
		let wts = wt.to_str().unwrap();
		assert_eq!(
			git(&["-C", wts, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/fresh"
		);
		assert!(!git_ok(&["-C", wts, "rev-parse", "--verify", "HEAD"]));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn orphan_of_an_existing_branch_is_branch_exists() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("orphan-exists-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "existing", &head]);
		let err = create(
			&req(
				&work,
				&base.join("wt"),
				CheckoutTarget::Orphan {
					name: BranchName::new("existing"),
				},
			),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(err, CreateError::BranchExists(ref n) if n == "existing"),
			"{fmt}: an orphan of an existing branch is refused, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn re_creating_the_exact_worktree_is_idempotent() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-idempotent-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();

		let first = create(&req(&work, &wt, new_branch("feature", start.clone())), None)
			.await
			.unwrap();
		let second = create(&req(&work, &wt, new_branch("feature", start)), None)
			.await
			.unwrap();
		assert_eq!(first, second, "{fmt}: re-create is idempotent");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn new_branch_is_strict_when_the_branch_exists() {
	// `NewBranch` is git `-b`: an existing ref (even at the requested start, with no worktree) is
	// BranchExists — adoption is unsafe. Completion of an interrupted attempt uses `ExistingBranch`.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("newbranch-strict-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &head]);
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let err = create(
			&req(&work, &base.join("wt"), new_branch("feature", start)),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(err, CreateError::BranchExists(ref n) if n == "feature"),
			"{fmt}: NewBranch is strict, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn existing_branch_completes_an_interrupted_attempt() {
	// A branch created but never checked out (an interrupted `-b`) is completed with `ExistingBranch`,
	// without re-creating the branch.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("create-complete-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &head]);
		let reflog_before = std::fs::read_to_string(work.join(".git/logs/refs/heads/feature")).ok();

		let wt = base.join("wt");
		create(&req(&work, &wt, existing_branch("feature", None)), None)
			.await
			.unwrap();

		let wts = wt.to_str().unwrap();
		assert_eq!(
			git(&["-C", wts, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feature"
		);
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head);
		let reflog_after = std::fs::read_to_string(work.join(".git/logs/refs/heads/feature")).ok();
		assert_eq!(reflog_before, reflog_after, "{fmt}: branch not re-created");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_worktree_from_a_bare_host_still_gets_its_head_reflog() {
	// git seeds a linked worktree's per-worktree `logs/HEAD` under the *non-bare* default even when the
	// host repository is bare (`core.logAllRefUpdates` unset) — the worktree is non-bare.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("bare-host-{fmt}"));
		let src = base.join("src");
		init_repo(&src, fmt);
		let head = commit_file(&src, "a.txt", "1\n", "init");
		let bare = base.join("bare.git");
		git(&[
			"clone",
			"--bare",
			"-q",
			src.to_str().unwrap(),
			bare.to_str().unwrap(),
		]);
		let b = bare.to_str().unwrap();
		assert_eq!(git(&["-C", b, "config", "core.bare"]).trim(), "true");

		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		create(&req_bare(&bare, &wt, new_branch("feature", start)), None)
			.await
			.unwrap();
		// The linked worktree's admin gets a HEAD reflog (non-bare default) ...
		assert!(
			bare.join("worktrees/wt/logs/HEAD").exists(),
			"{fmt}: the linked worktree gets logs/HEAD"
		);
		// ... while the *shared* branch reflog stays off (bare host default).
		assert!(
			!bare.join("logs/refs/heads/feature").exists(),
			"{fmt}: the shared branch reflog stays off"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_branch_checked_out_elsewhere() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-branchuse-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt1 = base.join("wt1");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			wt1.to_str().unwrap(),
			"-b",
			"feature",
		]);

		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let err = create(
			&req(&work, &base.join("wt2"), new_branch("feature", start)),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(
				err,
				CreateError::Refused(WorktreeClassification::BranchUseConflict { .. })
			),
			"{fmt}: expected a branch-use refusal, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_an_occupied_destination() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-destconflict-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let dest = base.join("occupied");
		std::fs::create_dir_all(&dest).unwrap();
		std::fs::write(dest.join("stuff"), b"x").unwrap();
		let err = create(&req(&work, &dest, new_branch("feature", start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				CreateError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: expected a destination refusal, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_present_worktree_that_differs_is_a_mismatch() {
	// A detached worktree at commit A; a request for detached commit B must not be treated as idempotent.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-mismatch-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let a = commit_file(&work, "a.txt", "1\n", "a");
		let b = commit_file(&work, "b.txt", "2\n", "b");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		// A detached worktree at `a`.
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"--detach",
			wt.to_str().unwrap(),
			&a,
		]);

		let target = CheckoutTarget::Detached {
			start: WorktreeObjectId::parse(kind, &b).unwrap(),
		};
		let err = create(&req(&work, &wt, target), None).await.unwrap_err();
		assert!(
			matches!(err, CreateError::ExistingWorktreeMismatch(_)),
			"{fmt}: a present worktree at a different commit is a mismatch, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn rejects_an_invalid_branch_name() {
	let base = unique_tmp("create-badname");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
	for bad in ["HEAD", "-foo", "x.lock", "with space"] {
		// Oracle: git rejects each as a branch name.
		assert!(
			!git_ok(&["check-ref-format", "--branch", bad]),
			"git rejects branch name {bad:?}"
		);
		let start = WorktreeObjectId::parse(gitana_object::HashKind::Sha1, &head).unwrap();
		let err = create(&req(&work, &base.join("wt"), new_branch(bad, start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(err, CreateError::InvalidBranchName(_)),
			"expected InvalidBranchName for {bad:?}, got {err:?}"
		);
	}
	let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn a_bogus_start_commit_writes_nothing() {
	// A correctly-sized but non-existent start object must fail *before* any ref or admin is published.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-badstart-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let bogus = match kind {
			gitana_object::HashKind::Sha1 => "f".repeat(40),
			gitana_object::HashKind::Sha256 => "f".repeat(64),
		};
		let start = WorktreeObjectId::parse(kind, &bogus).unwrap();
		let err = create(
			&req(&work, &base.join("wt"), new_branch("feature", start)),
			None,
		)
		.await
		.unwrap_err();
		assert!(matches!(err, CreateError::Failed(_)), "{fmt}: got {err:?}");
		// Nothing was published: no branch ref, no admin entry.
		assert!(!git_ok(&[
			"-C",
			w,
			"rev-parse",
			"--verify",
			"refs/heads/feature"
		]));
		assert!(
			!work.join(".git/worktrees").exists() || {
				std::fs::read_dir(work.join(".git/worktrees"))
					.map(|mut d| d.next().is_none())
					.unwrap_or(true)
			}
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_relative_destination() {
	let base = unique_tmp("create-relative");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
	let start = WorktreeObjectId::parse(gitana_object::HashKind::Sha1, &head).unwrap();
	let request = CreateRequest {
		repo: rid_at(&work),
		destination: std::path::PathBuf::from("relative/wt"),
		target: new_branch("feature", start),
	};
	assert!(create(&request, None).await.is_err());
	let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn an_orphan_gets_a_zero_entry_index() {
	// git writes a valid empty index for an orphan worktree immediately; assert we do too (so a `git
	// status` in the fresh orphan works without materialising the index lazily).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("orphan-index-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		create(
			&req(
				&work,
				&wt,
				CheckoutTarget::Orphan {
					name: BranchName::new("fresh"),
				},
			),
			None,
		)
		.await
		.unwrap();
		assert!(
			work.join(".git/worktrees/wt/index").exists(),
			"{fmt}: the orphan worktree has an index"
		);
		// git accepts the index: a status in the orphan runs cleanly (everything is untracked/empty).
		assert!(git_ok(&[
			"-C",
			wt.to_str().unwrap(),
			"status",
			"--porcelain"
		]));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn re_creating_after_the_branch_diverges_is_a_mismatch() {
	// A `NewBranch` worktree whose branch is later rewound onto history the requested start does not reach
	// must not be reported idempotent — its start relation is `Diverged`, so it is a mismatch.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-diverged-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let a = commit_file(&work, "a.txt", "1\n", "a");
		let b = commit_file(&work, "b.txt", "2\n", "b");
		let wt = base.join("wt");
		let b_id = WorktreeObjectId::parse(kind, &b).unwrap();

		create(&req(&work, &wt, new_branch("feature", b_id.clone())), None)
			.await
			.unwrap();
		// Rewind `feature` in its worktree to `a` — the requested start `b` is now unreachable from HEAD.
		let wts = wt.to_str().unwrap();
		git(&["-C", wts, "reset", "--hard", &a]);

		let err = create(&req(&work, &wt, new_branch("feature", b_id)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(err, CreateError::ExistingWorktreeMismatch(_)),
			"{fmt}: a diverged branch is a mismatch, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn existing_branch_reconciles_an_advanced_expected_start() {
	// Reconciling an interrupted create expected at `a`: the branch has since advanced to `b` (a
	// descendant), so it is *at or ahead of* the expected start and the create completes at the tip.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("existing-advanced-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let a = commit_file(&work, "a.txt", "1\n", "a");
		let b = commit_file(&work, "b.txt", "2\n", "b");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &b]);
		let wt = base.join("wt");
		let a_id = WorktreeObjectId::parse(kind, &a).unwrap();

		create(
			&req(&work, &wt, existing_branch("feature", Some(a_id))),
			None,
		)
		.await
		.unwrap();
		let wts = wt.to_str().unwrap();
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), b);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn existing_branch_refuses_a_diverged_expected_start() {
	// The reconciliation expected the branch at `b`, but it sits at `a` (which does not reach `b`): the
	// history has diverged, so the create is refused rather than checking out unexpected history.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("existing-diverged-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let a = commit_file(&work, "a.txt", "1\n", "a");
		let b = commit_file(&work, "b.txt", "2\n", "b");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &a]);
		let b_id = WorktreeObjectId::parse(kind, &b).unwrap();

		let err = create(
			&req(
				&work,
				&base.join("wt"),
				existing_branch("feature", Some(b_id)),
			),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(
				err,
				CreateError::Refused(WorktreeClassification::IdentityConflict { .. })
			),
			"{fmt}: a diverged expected start is refused, got {err:?}"
		);
		// Nothing was published — no worktree registration.
		assert!(
			!work.join(".git/worktrees/wt").exists(),
			"{fmt}: a refused reconcile writes no admin dir"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_force_duplicated_branch_is_a_branch_use_conflict_not_idempotent() {
	// A present worktree that otherwise matches the request, but whose branch has since been force-checked
	// out in *another* worktree, is a branch-use conflict — the check must precede the idempotent return.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("force-dup-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();

		create(&req(&work, &wt, new_branch("feature", start.clone())), None)
			.await
			.unwrap();
		// Force a second checkout of the same branch elsewhere.
		let wt2 = base.join("wt2");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"--force",
			wt2.to_str().unwrap(),
			"feature",
		]);

		let err = create(&req(&work, &wt, new_branch("feature", start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(
				err,
				CreateError::Refused(WorktreeClassification::BranchUseConflict { .. })
			),
			"{fmt}: a force-duplicated branch is a branch-use conflict, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn refuses_a_symlink_destination() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-symlink-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let target = base.join("as-link");
		symlink(&work, &target).unwrap();
		let err = create(&req(&work, &target, new_branch("feature", start)), None)
			.await
			.unwrap_err();
		assert!(matches!(err, CreateError::Refused(_)), "{fmt}: got {err:?}");
		let _ = std::fs::remove_dir_all(&base);
	}
}
