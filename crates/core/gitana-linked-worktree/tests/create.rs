//! Create / reconcile, oracle-checked against stock `git worktree add` and by having stock `git` operate
//! in the gitana-created worktree, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;

use common::*;
use gitana_linked_worktree::{
	BranchName, CheckoutTarget, CreateError, CreateRequest, Registration, RemovePolicy,
	RemoveRequest, RepositoryId, WorktreeClassification, WorktreeObjectId, create,
	durability_barrier_created, recover_prepared_create, remove,
};

fn req(work: &std::path::Path, dest: &std::path::Path, target: CheckoutTarget) -> CreateRequest {
	CreateRequest {
		repo: rid_at(work),
		destination: dest.to_path_buf(),
		target,
		committer: None,
		reflog_start: None,
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
		committer: None,
		reflog_start: None,
	}
}

fn new_branch(name: &str, start: WorktreeObjectId) -> CheckoutTarget {
	CheckoutTarget::NewBranch {
		name: BranchName::new(name),
		start,
		force_reset: false,
	}
}

fn reset_branch(name: &str, start: WorktreeObjectId) -> CheckoutTarget {
	CheckoutTarget::NewBranch {
		name: BranchName::new(name),
		start,
		force_reset: true,
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
async fn records_the_supplied_committer_and_start_token_in_reflogs() {
	// The caller-supplied `committer` is recorded on BOTH the branch-creation reflog and the new worktree's
	// `logs/HEAD` seed; the caller-supplied `reflog_start` token is used verbatim in the branch reflog message
	// (`branch: Created from HEAD`), never the resolved hash. (The `None` defaults — config/now committer and
	// the hash token — are covered by the other create tests.)
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-committer-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();

		let committer = "Tester <t@e> 1700000000 +0000";
		let request = CreateRequest {
			repo: rid_at(&work),
			destination: wt.clone(),
			target: new_branch("feature", start),
			committer: Some(committer.to_owned()),
			reflog_start: Some("HEAD".to_owned()),
		};
		create(&request, None).await.unwrap();

		// The branch-creation reflog records the supplied committer and the start TOKEN (`HEAD`), not the hash.
		let branch_reflog = std::fs::read_to_string(work.join(".git/logs/refs/heads/feature")).unwrap();
		assert!(
			branch_reflog.contains(committer),
			"{fmt}: branch reflog must record the supplied committer, got: {branch_reflog}"
		);
		assert!(
			branch_reflog.contains("branch: Created from HEAD"),
			"{fmt}: branch reflog message must use the start token, got: {branch_reflog}"
		);
		assert!(
			!branch_reflog.contains(&format!("Created from {head}")),
			"{fmt}: branch reflog message must not embed the resolved hash, got: {branch_reflog}"
		);

		// The new worktree's per-worktree `logs/HEAD` seed records the same supplied committer.
		let head_reflog = std::fs::read_to_string(work.join(".git/worktrees/wt/logs/HEAD")).unwrap();
		assert!(
			head_reflog.contains(committer),
			"{fmt}: worktree logs/HEAD must record the supplied committer, got: {head_reflog}"
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
async fn force_reset_moves_an_existing_branch() {
	// git `-B`: where plain `-b` refuses an existing branch (above), `force_reset` resets it to `start` and
	// checks it out. Verified by stock git operating in the result, plus git's `Reset to` reflog wording.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("newbranch-reset-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// `feature` sits at the first commit; a second commit advances `main` past it.
		git(&["-C", w, "branch", "feature", &first]);
		let second = commit_file(&work, "a.txt", "2\n", "second");
		assert_ne!(first, second);

		let wt = base.join("wt");
		let wts = wt.to_str().unwrap();
		let start = WorktreeObjectId::parse(kind, &second).unwrap();
		let insp = create(&req(&work, &wt, reset_branch("feature", start)), None)
			.await
			.unwrap();
		assert!(matches!(insp.registration, Registration::Present { .. }));

		// The branch was reset to the second commit and checked out on it — git agrees, and the checkout is clean.
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), second);
		assert_eq!(
			git(&["-C", wts, "rev-parse", "--abbrev-ref", "HEAD"]).trim(),
			"feature"
		);
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), second);
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());
		// The reflog records git's `-B` wording (`Reset to`), not `Created from`.
		let reflog = std::fs::read_to_string(work.join(".git/logs/refs/heads/feature")).unwrap();
		assert!(
			reflog.contains("branch: Reset to"),
			"{fmt}: expected a `Reset to` reflog line, got: {reflog}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_reset_refuses_a_symbolic_branch() {
	// git `-B alias` where `alias -> feature` derefs to the terminal (resetting it, dual reflogs, chain
	// locking) — a pathological case (a branch is ~never a symref). The library refuses it cleanly rather
	// than half-handling the deref, and mutates nothing.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("newbranch-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &first]);
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			"refs/heads/feature",
		]);
		let second = commit_file(&work, "a.txt", "2\n", "second");

		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &second).unwrap();
		let err = create(&req(&work, &wt, reset_branch("alias", start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(err, CreateError::UnsupportedSymbolicBranchReset(ref n) if n == "refs/heads/alias"),
			"{fmt}: expected a symbolic-ref refusal, got {err:?}"
		);
		// Nothing was mutated: `feature` is untouched and no worktree was registered.
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), first);
		assert!(!wt.exists(), "{fmt}: no checkout should have been created");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_reset_refuses_a_no_space_symref_branch() {
	// git accepts `ref:refs/heads/x` (no space after the colon) as a symref; the refusal checks the raw
	// `ref:` prefix so every git-valid spelling yields the matchable `UnsupportedSymbolicBranchReset`, not a
	// `Failed` error from the transaction layer.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("newbranch-nospace-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &first]);
		std::fs::write(
			work.join(".git/refs/heads/alias"),
			"ref:refs/heads/feature\n",
		)
		.unwrap();
		let second = commit_file(&work, "a.txt", "2\n", "second");

		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &second).unwrap();
		let err = create(&req(&work, &wt, reset_branch("alias", start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(err, CreateError::UnsupportedSymbolicBranchReset(ref n) if n == "refs/heads/alias"),
			"{fmt}: a no-space `ref:` symref must yield the matchable refusal, got {err:?}"
		);
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), first);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn force_reset_refuses_a_symlinked_branch() {
	// A branch whose ref file is a filesystem *symlink* is refused too (`read_symbolic` follows the link
	// and would miss it, so the refusal also checks for a symlinked ref file). This deliberately
	// over-refuses vs git — a symlink to a *bare* sibling like `feature` is a git *direct* ref it would
	// reset — but that legacy form is vanishingly rare, and refusing it defers the whole legacy-symlink-ref
	// surface safely.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("newbranch-symlink-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature", &first]);
		// `refs/heads/alias` as a filesystem symlink to the loose `feature` ref (git's legacy symref).
		symlink("feature", work.join(".git/refs/heads/alias")).unwrap();
		let second = commit_file(&work, "a.txt", "2\n", "second");

		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &second).unwrap();
		let err = create(&req(&work, &wt, reset_branch("alias", start)), None)
			.await
			.unwrap_err();
		assert!(
			matches!(err, CreateError::UnsupportedSymbolicBranchReset(ref n) if n == "refs/heads/alias"),
			"{fmt}: expected a symbolic-ref refusal for a legacy symlink, got {err:?}"
		);
		assert_eq!(git(&["-C", w, "rev-parse", "feature"]).trim(), first);
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
async fn prepared_recovery_recreates_an_exact_nonempty_checkout_missing_partial() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-recover-owned-partial-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		create(&req(&work, &wt, new_branch("feature", start.clone())), None)
			.await
			.unwrap();

		// Reproduce the exact commit order's interruption: admin, index, and checkout exist, but the final
		// checkout `.git` marker never became visible.
		std::fs::remove_file(wt.join(".git")).unwrap();
		assert!(wt.join("a.txt").is_file(), "{fmt}: partial is non-empty");
		let recovery = req(&work, &wt, existing_branch("feature", Some(start.clone())));

		let ordinary = create(&recovery, None).await.unwrap_err();
		assert!(
			matches!(
				ordinary,
				CreateError::Refused(WorktreeClassification::DestinationConflict { .. })
			),
			"{fmt}: ordinary create must still preserve unknown non-empty partials, got {ordinary:?}"
		);

		let inspection = recover_prepared_create(&recovery, None).await.unwrap();
		assert!(matches!(
			inspection.registration,
			Registration::Present { .. }
		));
		assert_eq!(
			git(&["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"]).trim(),
			head
		);
		assert!(git(&["-C", wt.to_str().unwrap(), "status", "--porcelain"]).is_empty());
		durability_barrier_created(&recovery).await.unwrap();
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn cancelling_prepared_recovery_does_not_abandon_the_owned_partial() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-recover-cancelled-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		create(&req(&work, &wt, new_branch("feature", start.clone())), None)
			.await
			.unwrap();
		std::fs::remove_file(wt.join(".git")).unwrap();

		let lock = work.join(".git/worktrees.lock");
		std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&lock)
			.unwrap();
		let recovery = req(&work, &wt, existing_branch("feature", Some(start)));
		let retained_request = recovery.clone();
		let caller =
			tokio::spawn(async move { recover_prepared_create(&retained_request, None).await });
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}
		assert!(
			!caller.is_finished(),
			"{fmt}: recovery is waiting on the lock"
		);
		caller.abort();
		assert!(caller.await.unwrap_err().is_cancelled());
		std::fs::remove_file(&lock).unwrap();

		let mut recovered = false;
		for _ in 0..100 {
			if wt.join(".git").is_file() {
				recovered = true;
				break;
			}
			tokio::task::spawn_blocking(|| {
				std::thread::sleep(std::time::Duration::from_millis(10));
			})
			.await
			.unwrap();
		}
		assert!(recovered, "{fmt}: retained recovery did not finish");
		assert_eq!(
			git(&["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"]).trim(),
			head
		);
		durability_barrier_created(&recovery).await.unwrap();
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn prepared_recovery_preserves_a_partial_when_the_baseline_does_not_match() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-recover-mismatch-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let first = commit_file(&work, "a.txt", "1\n", "first");
		let wt = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &first).unwrap();
		create(&req(&work, &wt, new_branch("feature", start)), None)
			.await
			.unwrap();
		std::fs::remove_file(wt.join(".git")).unwrap();
		let second = commit_file(&work, "b.txt", "2\n", "second");
		let wrong = WorktreeObjectId::parse(kind, &second).unwrap();

		let error = recover_prepared_create(
			&req(&work, &wt, existing_branch("feature", Some(wrong))),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(error, CreateError::Refused(_)),
			"{fmt}: mismatched recovery must fail closed, got {error:?}"
		);
		assert!(
			wt.join("a.txt").is_file(),
			"{fmt}: checkout content preserved"
		);
		assert!(
			work
				.join(".git/worktrees")
				.read_dir()
				.unwrap()
				.next()
				.is_some(),
			"{fmt}: registration preserved"
		);
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

/// An **unborn** branch checked out in another worktree conflicts for a *born-branch* create but not for an
/// *orphan* create — git's exact rule (probed 2.50.1): a born `-b <name>` reusing an unborn-elsewhere name is
/// refused "already used by worktree", while a second `--orphan -b <name>` on that name coexists. Guards
/// against over-broadening the use-conflict axis to blanket-allow every unborn-elsewhere create.
#[tokio::test]
async fn an_unborn_branch_elsewhere_blocks_a_born_create_but_not_an_orphan() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-unborn-elsewhere-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// An orphan worktree holds `orph` as an unborn branch (HEAD names `refs/heads/orph`, no ref yet).
		// `--orphan` needs a reasonably modern git; skip rather than fail if unavailable.
		let orph_wt = base.join("orph");
		if !git_ok(&[
			"-C",
			w,
			"worktree",
			"add",
			"--orphan",
			"-b",
			"orph",
			orph_wt.to_str().unwrap(),
		]) {
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}
		assert!(
			!git_ok(&["-C", w, "rev-parse", "--verify", "refs/heads/orph"]),
			"{fmt}: `orph` must be unborn"
		);

		// A born create (`-b orph`) reusing that unborn name is a use-conflict — the born branch would
		// collide with the orphan worktree's unborn HEAD.
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let born_err = create(
			&req(&work, &base.join("born"), new_branch("orph", start)),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(
				born_err,
				CreateError::Refused(WorktreeClassification::BranchUseConflict { .. })
			),
			"{fmt}: a born create reusing an unborn-elsewhere name must conflict, got {born_err:?}"
		);

		// A second orphan on the same unborn name is allowed — two orphans coexist.
		create(
			&req(
				&work,
				&base.join("orph2"),
				CheckoutTarget::Orphan {
					name: BranchName::new("orph"),
				},
			),
			None,
		)
		.await
		.unwrap();
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
		committer: None,
		reflog_start: None,
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
async fn sanitizes_a_pathological_basename_like_git() {
	// A destination basename with a newline (which would break the gitfile record delimiter) plus `~`/`:`
	// (refname-invalid): git sanitizes the *admin name* while keeping the real destination path. Assert we
	// pick the same admin name as stock git, the name has no delimiter byte, and git accepts our worktree.
	fn sole_admin(work: &std::path::Path) -> String {
		let dir = work.join(".git/worktrees");
		let mut names: Vec<String> = std::fs::read_dir(&dir)
			.unwrap()
			.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
			.collect();
		assert_eq!(names.len(), 1, "exactly one admin dir");
		names.pop().unwrap()
	}

	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-sanitize-{fmt}"));
		let work_git = base.join("gitrepo");
		let work_ours = base.join("ourrepo");
		init_repo(&work_git, fmt);
		init_repo(&work_ours, fmt);
		commit_file(&work_git, "a.txt", "1\n", "init");
		let head = commit_file(&work_ours, "a.txt", "1\n", "init");

		// `\n` is valid UTF-8 (and a legal Unix path byte), so both sides use the exact same basename.
		let bad = "wt\n~:x";
		let git_side = base.join("gside");
		let our_side = base.join("oside");
		std::fs::create_dir_all(&git_side).unwrap();
		std::fs::create_dir_all(&our_side).unwrap();
		let dest_git = git_side.join(bad);
		let dest_ours = our_side.join(bad);

		// Oracle: stock git.
		git(&[
			"-C",
			work_git.to_str().unwrap(),
			"worktree",
			"add",
			dest_git.to_str().unwrap(),
			"-b",
			"feat",
		]);
		let admin_git = sole_admin(&work_git);

		// Ours.
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		create(
			&req(&work_ours, &dest_ours, new_branch("feat", start)),
			None,
		)
		.await
		.unwrap();
		let admin_ours = sole_admin(&work_ours);

		assert_eq!(admin_ours, admin_git, "{fmt}: admin name matches git");
		assert!(
			!admin_ours.contains('\n') && !admin_ours.contains('\r'),
			"{fmt}: admin name has no gitfile delimiter: {admin_ours:?}"
		);
		// git accepts our worktree: the cross-pointers resolve and HEAD is the requested branch/commit.
		let dst = dest_ours.to_str().unwrap();
		assert_eq!(git(&["-C", dst, "rev-parse", "HEAD"]).trim(), head);
		assert_eq!(
			git(&["-C", dst, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feat"
		);
		assert!(git(&["-C", dst, "status", "--porcelain"]).is_empty());
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn accepts_a_non_utf8_destination_and_round_trips() {
	// Requirement (docs/code-henge-linked-worktree-requirements.md): identity/operation paths are accepted
	// as **native** paths without UTF-8 conversion. A non-UTF-8 destination is created; its cross-pointers
	// round-trip **byte-clean** (an idempotent re-create reads them back, and stock git operates in the
	// worktree); and it removes cleanly. A filesystem that rejects non-UTF-8 filenames (macOS APFS/HFS+)
	// can't host the scenario — probe and skip there; the check runs for real on Linux/ext4.
	use std::os::unix::ffi::OsStrExt;
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("create-nonutf8-ok-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		// `0xff` is not valid UTF-8 but is a legal Unix path byte. Probe filesystem support and skip if the
		// name can't be created at all.
		let dest = base.join(std::ffi::OsStr::from_bytes(b"wt\xffx"));
		if std::fs::create_dir(&dest).is_err() {
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}
		std::fs::remove_dir(&dest).unwrap(); // `create` materialises it itself

		// Create is accepted (not refused on UTF-8 grounds) and reports the worktree present.
		let inspection = create(
			&req(&work, &dest, new_branch("feature", start.clone())),
			None,
		)
		.await
		.expect("non-UTF-8 destination accepted");
		assert!(
			matches!(inspection.registration, Registration::Present { .. }),
			"{fmt}: created at the non-UTF-8 destination"
		);

		// The worktree is **rediscoverable** through the public identity API: discovery parses the non-UTF-8
		// `.git` and `commondir` pointers byte-clean and resolves the shared common dir (identity is the
		// common dir, so this equals the repository's own id).
		let discovered = RepositoryId::discover(&dest)
			.await
			.expect("rediscovered from the non-UTF-8 linked worktree");
		assert_eq!(
			discovered,
			rid_at(&work),
			"{fmt}: discovery resolves the same repository identity"
		);

		// The pointer files round-trip byte-clean: a repeat create is the idempotent no-op — which reads the
		// admin `gitdir` and the checkout `.git` back — and stock git can operate in the worktree, resolving
		// its non-UTF-8 gitfile/admin path.
		assert!(
			create(&req(&work, &dest, new_branch("feature", start)), None)
				.await
				.is_ok(),
			"{fmt}: idempotent re-create over the non-UTF-8 pointers"
		);
		let out = std::process::Command::new("git")
			.arg("-C")
			.arg(&dest)
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{fmt}: stock git operates in the gta-created non-UTF-8 worktree"
		);
		assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), head);

		// And safe removal round-trips the same non-UTF-8 identity.
		remove(&RemoveRequest {
			repo: rid_at(&work),
			destination: dest.clone(),
			expected_branch: None,
			policy: RemovePolicy::Conservative,
		})
		.await
		.expect("non-UTF-8 worktree removed");
		assert!(!dest.exists(), "{fmt}: checkout gone after remove");

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
