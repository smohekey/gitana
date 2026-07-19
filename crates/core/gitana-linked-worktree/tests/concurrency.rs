//! Registration-level lock/CAS (slice 5): a lost race on a worktree registration is a **conflict, not a
//! duplicate/overwrite**. Exercised with genuine OS-thread concurrency (each thread drives an async
//! `create`/`remove` on its own current-thread runtime), over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{
	CheckoutTarget, CreateError, CreateRequest, LinkedWorktreeError, RemovePolicy, RemoveRequest,
	RepositoryId, WorktreeObjectId, create, remove,
};

fn detached(start: WorktreeObjectId) -> CheckoutTarget {
	CheckoutTarget::Detached { start }
}

fn create_req(repo: RepositoryId, dest: &std::path::Path, target: CheckoutTarget) -> CreateRequest {
	CreateRequest {
		repo,
		destination: dest.to_path_buf(),
		target,
		committer: None,
		reflog_start: None,
	}
}

/// Drive `future` to completion on a fresh current-thread runtime (no `time`/`io` drivers needed — the
/// crate's blocking `std::fs` and thread-sleep backoff don't use tokio timers).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
	tokio::runtime::Builder::new_current_thread()
		.build()
		.unwrap()
		.block_on(future)
}

#[tokio::test]
async fn concurrent_creates_do_not_duplicate_the_registration() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("concur-create-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let dest = base.join("wt");
		let repo = rid_at(&work);

		// N OS threads race to create the *same* detached worktree at the same destination. (Detached — not
		// NewBranch — because a NewBranch's ref CAS already masks the race; a detached target has no branch
		// CAS, so only the registration lock stops a loser from writing a *second* admin dir.)
		let n = 8;
		let handles: Vec<_> = (0..n)
			.map(|_| {
				let (repo, dest, start) = (repo.clone(), dest.clone(), start.clone());
				std::thread::spawn(move || {
					block_on(create(&create_req(repo, &dest, detached(start)), None))
				})
			})
			.collect();
		let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

		// Every result is success (established, or an idempotent no-op) or a structured registration-lock
		// conflict — never a panic, a corruption, or any other error. At least one established the worktree.
		assert!(
			results.iter().any(Result::is_ok),
			"{fmt}: at least one create established the worktree"
		);
		for r in &results {
			if let Err(e) = r {
				assert!(
					matches!(
						e,
						CreateError::Failed(LinkedWorktreeError::RegistrationLocked(_))
					),
					"{fmt}: the only allowed failure is a registration-lock conflict, got {e:?}"
				);
			}
		}

		// The invariant the lock protects: **exactly one** admin registration for the destination (no
		// duplicate/orphan), the lock file was cleaned up, and stock git sees one healthy detached worktree.
		let admins = std::fs::read_dir(work.join(".git/worktrees"))
			.map(|d| d.count())
			.unwrap_or(0);
		assert_eq!(
			admins, 1,
			"{fmt}: exactly one admin registration, no duplicate"
		);
		assert!(
			!work.join(".git/worktrees.lock").exists(),
			"{fmt}: the registration lock is released"
		);
		let wts = dest.to_str().unwrap();
		assert_eq!(git(&["-C", wts, "rev-parse", "HEAD"]).trim(), head);
		assert!(git(&["-C", wts, "status", "--porcelain"]).is_empty());

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn same_runtime_contention_makes_progress_without_stalling() {
	// Two creates for the same destination on ONE current-thread runtime (`tokio::join!`). The lock *holder*
	// must keep making progress while the *waiter* backs off — the retry backoff must `.await` a real
	// suspension point. A blocking (non-yielding) backoff would freeze the shared executor so the holder
	// can never release, forcing the waiter to a false `RegistrationLocked` after the full retry budget.
	// Expect: both resolve `Ok` (one establishes, one idempotent), exactly one registration, and quickly.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("same-rt-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let dest = base.join("wt");
		let repo = rid_at(&work);

		let req_a = create_req(repo.clone(), &dest, detached(start.clone()));
		let req_b = create_req(repo.clone(), &dest, detached(start.clone()));
		let (ra, rb) = tokio::join!(create(&req_a, None), create(&req_b, None));

		assert!(
			ra.is_ok() && rb.is_ok(),
			"{fmt}: both same-runtime creates resolve (holder progressed during the waiter's backoff): a={ra:?}, b={rb:?}"
		);
		let admins = std::fs::read_dir(work.join(".git/worktrees"))
			.map(|d| d.count())
			.unwrap_or(0);
		assert_eq!(admins, 1, "{fmt}: exactly one registration");
		assert!(
			!work.join(".git/worktrees.lock").exists(),
			"{fmt}: the registration lock is released"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_held_registration_lock_surfaces_as_a_retryable_conflict() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("held-lock-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let dest = base.join("wt");
		let repo = rid_at(&work);
		let lock = work.join(".git/worktrees.lock");

		// Simulate a concurrent operation holding the lock.
		std::fs::write(&lock, b"").unwrap();
		let err = create(
			&create_req(repo.clone(), &dest, detached(start.clone())),
			None,
		)
		.await
		.unwrap_err();
		assert!(
			matches!(
				err,
				CreateError::Failed(LinkedWorktreeError::RegistrationLocked(_))
			),
			"{fmt}: a held lock is a structured conflict, got {err:?}"
		);
		// Fail-closed: nothing was written while the lock was held.
		let w = work.to_str().unwrap();
		assert!(
			!dest.exists(),
			"{fmt}: no checkout written under a held lock"
		);
		assert!(
			std::fs::read_dir(work.join(".git/worktrees"))
				.map(|mut d| d.next().is_none())
				.unwrap_or(true),
			"{fmt}: no admin written under a held lock"
		);

		// Releasing the lock lets the same request succeed — the conflict is retryable.
		std::fs::remove_file(&lock).unwrap();
		create(&create_req(repo, &dest, detached(start)), None)
			.await
			.unwrap();
		assert_eq!(
			git(&["-C", dest.to_str().unwrap(), "rev-parse", "HEAD"]).trim(),
			head
		);
		assert!(
			!lock.exists(),
			"{fmt}: lock released after the successful create"
		);
		let _ = w;

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn concurrent_create_and_remove_reach_a_consistent_state() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("concur-cr-rm-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let dest = base.join("wt");
		let repo = rid_at(&work);

		// Establish the worktree, then race a second create against a remove of the same destination.
		create(
			&create_req(repo.clone(), &dest, detached(start.clone())),
			None,
		)
		.await
		.unwrap();

		let creater = {
			let (repo, dest, start) = (repo.clone(), dest.clone(), start.clone());
			std::thread::spawn(move || block_on(create(&create_req(repo, &dest, detached(start)), None)))
		};
		let remover = {
			let (repo, dest) = (repo.clone(), dest.clone());
			std::thread::spawn(move || {
				block_on(remove(&RemoveRequest {
					repo,
					destination: dest,
					expected_branch: None,
					policy: RemovePolicy::Conservative,
				}))
			})
		};
		let create_res = creater.join().unwrap();
		let remove_res = remover.join().unwrap();

		// Neither corrupts: each is a structured Ok/refusal/conflict (no panic). The end state is coherent —
		// either the worktree is present (create won the last write) or absent (remove won) — and never a
		// duplicate registration or a leftover lock, whichever way the race resolved.
		let _ = (&create_res, &remove_res);
		let admins = std::fs::read_dir(work.join(".git/worktrees"))
			.map(|d| d.count())
			.unwrap_or(0);
		assert!(
			admins <= 1,
			"{fmt}: at most one admin registration after the race (got {admins})"
		);
		assert!(
			!work.join(".git/worktrees.lock").exists(),
			"{fmt}: the registration lock is released"
		);
		// If a registration remains, it is a healthy detached worktree git can read.
		if admins == 1 && dest.join(".git").is_file() {
			assert_eq!(
				git(&["-C", dest.to_str().unwrap(), "rev-parse", "HEAD"]).trim(),
				head
			);
		}

		let _ = std::fs::remove_dir_all(&base);
	}
}
