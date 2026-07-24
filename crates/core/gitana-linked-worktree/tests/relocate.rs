//! Safe move of a linked worktree, oracle-checked against stock `git worktree move`/`list` and by having
//! stock `git` operate on the worktree before/after, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{
	BranchName, ProtectionReason, RelocateError, RelocateOutcome, RelocateRequest,
	WorktreeClassification, relocate,
};

/// `git`, trimmed — the shared `common::git` returns raw stdout (trailing newline), so trim for value
/// comparisons against hashes/refs.
fn g(args: &[&str]) -> String {
	git(args).trim().to_owned()
}

fn req(
	work: &std::path::Path,
	from: &std::path::Path,
	to: &std::path::Path,
	branch: Option<&str>,
) -> RelocateRequest {
	req_force(work, from, to, branch, 0)
}

fn req_force(
	work: &std::path::Path,
	from: &std::path::Path,
	to: &std::path::Path,
	branch: Option<&str>,
	force: u8,
) -> RelocateRequest {
	RelocateRequest {
		repo: rid_at(work),
		from: from.to_path_buf(),
		to: to.to_path_buf(),
		expected_branch: branch.map(BranchName::new),
		force,
	}
}

/// The single admin id under `<work>/.git/worktrees` (the `git worktree` id). Panics unless exactly one
/// linked worktree is registered — the fixtures here register exactly one.
fn admin_id(work: &std::path::Path) -> String {
	let dir = work.join(".git").join("worktrees");
	let mut ids: Vec<_> = std::fs::read_dir(&dir)
		.expect("worktrees dir")
		.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(ids.len(), 1, "exactly one registered worktree");
	ids.pop().unwrap()
}

/// The canonical checkout paths stock `git worktree list --porcelain` reports (the primary first).
fn git_listed_paths(work: &std::path::Path) -> Vec<String> {
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"list",
		"--porcelain",
	])
	.lines()
	.filter_map(|l| l.strip_prefix("worktree "))
	.map(str::to_owned)
	.collect()
}

#[tokio::test]
async fn relocate_moves_the_checkout_and_git_agrees() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("relocate-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let head = commit_file(&work, "a.txt", "1\n", "init");

		// Build the worktree the oracle way: stock `git worktree add -b feature <from>`.
		let from = base.join("from");
		git(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			from.to_str().unwrap(),
		]);
		let id_before = admin_id(&work);

		let to = base.join("to");
		let outcome = relocate(&req(&work, &from, &to, None))
			.await
			.expect("relocate");
		assert_eq!(
			outcome,
			RelocateOutcome::Relocated {
				from: from.clone(),
				to: to.clone(),
			}
		);

		// The checkout moved; the old path is gone, the new one is a working checkout.
		assert!(!from.exists(), "old checkout path gone");
		assert_eq!(g(&["-C", to.to_str().unwrap(), "rev-parse", "HEAD"]), head);
		assert_eq!(
			g(&["-C", to.to_str().unwrap(), "symbolic-ref", "HEAD"]),
			"refs/heads/feature"
		);
		// Its working tree is clean and operable under stock git (the pointers are consistent).
		assert_eq!(
			g(&["-C", to.to_str().unwrap(), "status", "--porcelain"]),
			""
		);

		// Identity preserved: the admin id (git worktree id) is unchanged by the move.
		assert_eq!(
			admin_id(&work),
			id_before,
			"admin id stable across the move"
		);

		// Stock git's own worktree list reports the new path and not the old.
		let listed = git_listed_paths(&work);
		assert!(
			listed.contains(&canonical(&to).to_string_lossy().into_owned()),
			"new path listed"
		);
		assert!(
			!listed.contains(&canonical(&from).to_string_lossy().into_owned()),
			"old path not listed",
		);
	}
}

#[tokio::test]
async fn relocate_supports_a_nested_destination_when_the_parent_exists() {
	// The session-label use case: move `<guid>` to a nested branch-shaped label. relocate is git-faithful —
	// it does not create intermediate dirs, so the caller makes the parent first.
	let base = unique_tmp("relocate-nested");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let from = base.join("guid");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature/login",
		from.to_str().unwrap(),
	]);

	let nested = work.join(".codehenge/worktrees/feature/login");
	std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
	relocate(&req(&work, &from, &nested, None))
		.await
		.expect("relocate nested");
	assert_eq!(
		g(&["-C", nested.to_str().unwrap(), "symbolic-ref", "HEAD"]),
		"refs/heads/feature/login"
	);
}

#[tokio::test]
async fn relocate_moves_a_dirty_worktree_intact() {
	// A move relocates files; it is not a cleanliness decision — a dirty worktree rides along (git's behaviour).
	let base = unique_tmp("relocate-dirty");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);
	std::fs::write(from.join("a.txt"), "dirty\n").unwrap();

	let to = base.join("to");
	relocate(&req(&work, &from, &to, None))
		.await
		.expect("relocate dirty");
	assert_eq!(
		std::fs::read_to_string(to.join("a.txt")).unwrap(),
		"dirty\n",
		"dirty content preserved"
	);
}

#[tokio::test]
async fn relocate_is_idempotent_when_from_equals_to() {
	let base = unique_tmp("relocate-idem");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);

	let outcome = relocate(&req(&work, &from, &from, None))
		.await
		.expect("idempotent");
	assert_eq!(outcome, RelocateOutcome::AlreadyAt { to: from.clone() });
	assert!(from.exists(), "the worktree is untouched");
}

#[tokio::test]
async fn relocate_refuses_the_primary_worktree() {
	let base = unique_tmp("relocate-primary");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let to = base.join("elsewhere");
	let err = relocate(&req(&work, &work, &to, None))
		.await
		.expect_err("refuses primary");
	assert!(matches!(err, RelocateError::IsPrimaryWorktree(_)));
	assert!(!to.exists(), "nothing was moved");
}

#[tokio::test]
async fn relocate_refuses_a_locked_worktree() {
	let base = unique_tmp("relocate-locked");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"lock",
		from.to_str().unwrap(),
	]);

	let to = base.join("to");
	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses locked");
	assert!(matches!(
		err,
		RelocateError::Refused(WorktreeClassification::ProtectedWithReason {
			reason: ProtectionReason::Locked { .. }
		})
	));
	assert!(from.exists() && !to.exists(), "the worktree stays put");
}

#[tokio::test]
async fn relocate_refuses_an_occupied_destination() {
	let base = unique_tmp("relocate-occupied");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);

	let to = base.join("to");
	std::fs::create_dir_all(&to).unwrap();
	std::fs::write(to.join("other.txt"), "x").unwrap();
	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses occupied dest");
	assert!(matches!(err, RelocateError::DestinationOccupied { .. }));
	assert!(from.exists(), "the source is untouched");
	assert_eq!(
		std::fs::read_to_string(to.join("other.txt")).unwrap(),
		"x",
		"the destination is untouched"
	);
}

#[tokio::test]
async fn relocate_refuses_a_destination_with_a_stale_registration() {
	// `to` is absent on disk but still named by a prunable registration (its checkout was deleted). Moving
	// there would leave two admins naming one checkout — refuse.
	let base = unique_tmp("relocate-staledest");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let to = base.join("to");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"a",
		to.to_str().unwrap(),
	]);
	std::fs::remove_dir_all(&to).unwrap(); // leaves a prunable registration naming `to`

	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"b",
		from.to_str().unwrap(),
	]);

	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses stale-registered dest");
	assert!(matches!(err, RelocateError::DestinationRegistered { .. }));
	assert!(from.exists(), "the source is untouched");
}

#[tokio::test]
async fn relocate_preserves_a_relative_checkout_pointer_across_depths() {
	// A worktree with a *relative* checkout `.git` (git's `worktree.useRelativePaths`) breaks if moved to a
	// different depth unless the pointer is recomputed. relocate preserves the relative style, recomputing it
	// for the new depth, so git works at the new location.
	let base = unique_tmp("relocate-relative");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);

	// Rewrite the checkout `.git` to a *relative* pointer (as `worktree.useRelativePaths` would): from
	// `<base>/from`, `../repo/.git/worktrees/<id>` resolves to the admin dir.
	let id = admin_id(&work);
	std::fs::write(
		from.join(".git"),
		format!("gitdir: ../repo/.git/worktrees/{id}\n"),
	)
	.unwrap();
	assert_eq!(
		g(&[
			"-C",
			from.to_str().unwrap(),
			"rev-parse",
			"--abbrev-ref",
			"HEAD"
		]),
		"feature"
	);

	// Move to a *different depth* (nested inside the repo). Without a checkout-pointer rewrite the relative
	// path would now resolve to the wrong place.
	let nested = work.join(".codehenge/worktrees/x/y");
	std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
	relocate(&req(&work, &from, &nested, None))
		.await
		.expect("relocate a relative-pointer worktree");
	assert_eq!(
		g(&["-C", nested.to_str().unwrap(), "status", "--porcelain"]),
		"",
		"git works at the new depth"
	);
	assert!(
		std::fs::read_to_string(nested.join(".git"))
			.unwrap()
			.starts_with("gitdir: .."),
		"the checkout pointer stayed relative, recomputed for the new depth",
	);
}

#[tokio::test]
async fn relocate_force_drops_a_stale_destination_registration() {
	// With force, a destination registered to a since-deleted (prunable) worktree is moved onto — the stale
	// registration is dropped, so the repository never ends up with two admins for one checkout path.
	let base = unique_tmp("relocate-force-stale");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let to = base.join("to");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"a",
		to.to_str().unwrap(),
	]);
	std::fs::remove_dir_all(&to).unwrap(); // prunable registration naming `to`

	let from = base.join("from");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"feature",
		from.to_str().unwrap(),
	]);

	relocate(&req_force(&work, &from, &to, None, 1))
		.await
		.expect("force move onto stale dest");
	assert_eq!(
		g(&["-C", to.to_str().unwrap(), "symbolic-ref", "HEAD"]),
		"refs/heads/feature"
	);
	// Exactly one registration remains for the destination (the moved worktree); git lists it once.
	let listed = git_listed_paths(&work);
	assert_eq!(
		listed.iter().filter(|p| p.ends_with("/to")).count(),
		1,
		"no duplicate registration for the destination",
	);
}

#[tokio::test]
async fn relocate_from_equals_to_still_validates_the_source() {
	// The idempotent `from == to` no-op must only apply to a genuine, movable worktree — not to an absent or
	// primary source.
	let base = unique_tmp("relocate-idem-valid");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let ghost = base.join("ghost");
	assert!(matches!(
		relocate(&req(&work, &ghost, &ghost, None)).await,
		Err(RelocateError::Refused(_))
	));
	assert!(matches!(
		relocate(&req(&work, &work, &work, None)).await,
		Err(RelocateError::IsPrimaryWorktree(_))
	));
}

#[tokio::test]
async fn relocate_refuses_an_absent_source() {
	let base = unique_tmp("relocate-absent");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let from = base.join("never-created");
	let to = base.join("to");
	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses absent source");
	assert!(matches!(err, RelocateError::Refused(_)));
	assert!(!to.exists(), "nothing was moved");
}
