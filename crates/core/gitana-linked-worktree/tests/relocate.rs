//! Safe move of a linked worktree, oracle-checked against stock `git worktree move`/`list` and by having
//! stock `git` operate on the worktree before/after, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{
	BranchName, DestinationKind, ProtectionReason, RelocateError, RelocateOutcome, RelocateRequest,
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
async fn relocate_refuses_a_foreign_registration_at_the_destination() {
	// An admin whose `gitdir` names `to` but whose ownership (`commondir`) is broken is still listed by git;
	// relocating onto it without force would leave a duplicate registration, so it must be refused. (An
	// ownership-filtered scan would miss it.)
	let base = unique_tmp("relocate-foreign-dest");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	// Register both worktrees while healthy (git refuses to add one once another's commondir is broken).
	let to = base.join("to");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"other",
		to.to_str().unwrap(),
	]);
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

	// Now break `to`'s `commondir` so ownership no longer matches this repo, and remove its checkout — a
	// prunable, foreign-looking registration that still names `to` (git lists it; its gitdir is intact).
	std::fs::write(
		work.join(".git/worktrees/to/commondir"),
		"/nonexistent/gitdir\n",
	)
	.unwrap();
	std::fs::remove_dir_all(&to).unwrap();

	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses foreign registration");
	assert!(
		matches!(err, RelocateError::DestinationRegistered { .. }),
		"got {err:?}"
	);
	assert!(from.exists(), "the source is untouched");
}

#[tokio::test]
async fn relocate_ignores_a_stray_non_admin_entry_under_worktrees() {
	// A harmless non-admin child of worktrees/ (e.g. `.DS_Store`) is not a git-listed registration and must
	// not block the move by failing when its (nonexistent) backlink is read.
	let base = unique_tmp("relocate-stray");
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
	std::fs::write(work.join(".git/worktrees/.DS_Store"), b"junk").unwrap();

	let to = base.join("to");
	relocate(&req(&work, &from, &to, None))
		.await
		.expect("a stray entry does not block the move");
	assert_eq!(
		g(&["-C", to.to_str().unwrap(), "symbolic-ref", "HEAD"]),
		"refs/heads/feature"
	);
}

#[tokio::test]
async fn relocate_refuses_an_untrusted_symlinked_registration() {
	// A symlinked admin leaf under worktrees/ is git-listed but cannot be read no-follow — it might name the
	// destination, so relocate fails closed rather than dereference it or ignore it.
	let base = unique_tmp("relocate-untrusted");
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

	// Plant a symlinked admin under worktrees/.
	std::os::unix::fs::symlink(base.join("elsewhere"), work.join(".git/worktrees/planted")).unwrap();

	let to = base.join("to");
	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses untrusted registration");
	assert!(
		matches!(err, RelocateError::UntrustedRegistration(_)),
		"got {err:?}"
	);
	assert!(from.exists() && !to.exists(), "nothing was moved");
}

#[tokio::test]
async fn relocate_refuses_a_destination_inside_a_registration_admin() {
	// A stale registration whose admin dir encloses the destination cannot be dropped — doing so would delete
	// the just-moved checkout. Refused before renaming, no force overrides.
	let base = unique_tmp("relocate-enclosed-dest");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");

	let victim = base.join("victim");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"victim",
		victim.to_str().unwrap(),
	]);
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

	// Point victim's gitdir at a path *inside* its own admin dir, and remove its checkout (prunable).
	let victim_admin = work.join(".git/worktrees/victim");
	let enclosed_to = victim_admin.join("nested");
	// The *admin's* gitdir holds a bare path to the checkout's `.git` (no `gitdir: ` prefix — that is on the
	// checkout side).
	std::fs::write(
		victim_admin.join("gitdir"),
		format!("{}/.git\n", enclosed_to.display()),
	)
	.unwrap();
	std::fs::remove_dir_all(&victim).unwrap();

	let err = relocate(&req_force(&work, &from, &enclosed_to, None, 2))
		.await
		.expect_err("refuses enclosing registration");
	assert!(
		matches!(err, RelocateError::DestinationInsideRegistration { .. }),
		"got {err:?}"
	);
	assert!(from.exists(), "the source is untouched");
}

#[tokio::test]
async fn relocate_refuses_a_destination_with_a_malformed_git_file() {
	// A non-empty destination holding a malformed regular `.git` is occupied — reported as DestinationOccupied
	// (git's "already exists"), not a metadata-parse Failed. Occupancy must not parse the target `.git`.
	let base = unique_tmp("relocate-malformed-dest");
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
	std::fs::write(to.join(".git"), "garbage not a gitfile\n").unwrap();

	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("occupied, not failed");
	assert!(
		matches!(err, RelocateError::DestinationOccupied { .. }),
		"got {err:?}"
	);
	assert!(from.exists(), "the source is untouched");
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

#[tokio::test]
async fn relocate_completes_when_the_checkout_directory_is_read_only() {
	// A read-only checkout *directory* (mode 0555) whose `.git` file is writable (0644) must still move — the
	// checkout `.git` is rewritten in place (file-write only), not via a temp sibling that would need the
	// directory writable and wrongly report the move Incomplete *after* the rename. Matches stock git, which
	// completes the move. The `.git`'s own permissions are preserved.
	use std::os::unix::fs::PermissionsExt;
	let base = unique_tmp("relocate-ro-dir");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
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

	// Pin the `.git` file to 0644 and lock the checkout directory to read-only+execute (0555).
	std::fs::set_permissions(from.join(".git"), std::fs::Permissions::from_mode(0o644)).unwrap();
	std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o555)).unwrap();

	let to = base.join("to");
	let outcome = relocate(&req(&work, &from, &to, None))
		.await
		.expect("relocate completes despite the read-only checkout directory");
	assert_eq!(
		outcome,
		RelocateOutcome::Relocated {
			from: from.clone(),
			to: to.clone(),
		}
	);

	// The `.git` pointer's permissions survived the in-place rewrite (not reset by a replacing rename).
	let mode = std::fs::metadata(to.join(".git"))
		.unwrap()
		.permissions()
		.mode();
	assert_eq!(mode & 0o777, 0o644, "checkout .git mode preserved");

	// Restore write so stock git can operate, then confirm the moved pointer is valid and listed.
	std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
	assert_eq!(g(&["-C", to.to_str().unwrap(), "rev-parse", "HEAD"]), head);
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_handles_a_destination_that_routes_through_the_source() {
	// The CLI, invoked *inside* the worktree with a relative `../moved`, forms `<from>/../moved` — a path that
	// resolves only while `from` exists. relocate must pin the destination before the rename and leave a valid
	// registration, not a broken one pointing through the vanished source.
	let base = unique_tmp("relocate-through-source");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
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

	// `<from>/../moved` — absolute, but reaching its parent through `from`.
	let through = from.join("..").join("moved");
	let landing = base.join("moved");
	let outcome = relocate(&req(&work, &from, &through, None))
		.await
		.expect("relocate resolves the destination and completes");
	assert_eq!(
		outcome,
		RelocateOutcome::Relocated {
			from: from.clone(),
			to: through.clone(),
		}
	);

	// The checkout landed where the path resolves, with a valid (resolvable) registration — stock git agrees.
	assert!(!from.exists(), "old checkout path gone");
	assert_eq!(
		g(&["-C", landing.to_str().unwrap(), "rev-parse", "HEAD"]),
		head
	);
	assert_eq!(
		g(&["-C", landing.to_str().unwrap(), "status", "--porcelain"]),
		""
	);
	assert!(
		git_listed_paths(&work).contains(&canonical(&landing).to_string_lossy().into_owned()),
		"new path listed and resolvable",
	);
}

#[tokio::test]
async fn relocate_completes_when_the_admin_directory_is_read_only() {
	// Mirror of the read-only-checkout-dir case for the admin backlink: a read-only admin *directory* (0555)
	// whose `gitdir` file is writable must still move — the backlink is rewritten in place (as stock git does,
	// verified keeping the file's inode and mode), not via a temp sibling that would need the admin directory
	// writable and wrongly report the move Incomplete after the rename.
	use std::os::unix::fs::{MetadataExt, PermissionsExt};
	let base = unique_tmp("relocate-ro-admin");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
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

	let admin = work.join(".git").join("worktrees").join(admin_id(&work));
	let backlink = admin.join("gitdir");
	let ino_before = std::fs::metadata(&backlink).unwrap().ino();
	std::fs::set_permissions(&backlink, std::fs::Permissions::from_mode(0o644)).unwrap();
	std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o555)).unwrap();

	let to = base.join("to");
	let outcome = relocate(&req(&work, &from, &to, None))
		.await
		.expect("relocate completes despite the read-only admin directory");
	assert_eq!(
		outcome,
		RelocateOutcome::Relocated {
			from: from.clone(),
			to: to.clone(),
		}
	);

	// The backlink was rewritten in place: same inode, mode preserved — not replaced by a fresh temp.
	std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o755)).unwrap();
	let meta = std::fs::metadata(&backlink).unwrap();
	assert_eq!(
		meta.ino(),
		ino_before,
		"admin gitdir updated in place (same inode)"
	);
	assert_eq!(
		meta.permissions().mode() & 0o777,
		0o644,
		"admin gitdir mode preserved"
	);

	assert_eq!(g(&["-C", to.to_str().unwrap(), "rev-parse", "HEAD"]), head);
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_refuses_a_source_with_no_head() {
	// A registered, cross-pointer-consistent worktree whose `<admin>/HEAD` is absent is an invalid partial:
	// stock `git worktree move` rejects it, and so must relocate — reporting Refused, not Relocated.
	let base = unique_tmp("relocate-no-head");
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

	// Delete the admin HEAD, leaving the registration and cross-pointers intact but the source HEAD-less.
	let admin = work.join(".git").join("worktrees").join(admin_id(&work));
	std::fs::remove_file(admin.join("HEAD")).unwrap();

	// Oracle: stock git also refuses to move this source.
	let to = base.join("to");
	assert!(
		!git_ok(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"move",
			from.to_str().unwrap(),
			to.to_str().unwrap(),
		]),
		"stock git refuses the HEAD-less move",
	);
	assert!(!to.exists(), "the oracle moved nothing");

	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("refuses a HEAD-less source");
	assert!(matches!(err, RelocateError::Refused(_)), "got {err:?}");
	assert!(from.exists(), "the source is untouched");
	assert!(!to.exists(), "nothing was moved");
}

#[tokio::test]
async fn relocate_moves_a_source_with_a_cyclic_head_symref() {
	// `git worktree move` never resolves HEAD's ref chain, so a *structurally valid* symbolic HEAD pointing at
	// a cyclic (or otherwise unresolvable) chain is still movable — verified against stock git, which moves it
	// and lists HEAD as all-zero. relocate reads HEAD structurally (`resolve_head: false`), so it matches;
	// resolving the chain, as a full inspection does, would wrongly fail the move with a malformed-HEAD error.
	let base = unique_tmp("relocate-cyclic-head");
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

	// A self-cyclic symref, with the worktree HEAD pointing into it — structurally valid, unresolvable.
	std::fs::write(work.join(".git/refs/heads/a"), "ref: refs/heads/a\n").unwrap();
	let admin = work.join(".git").join("worktrees").join(admin_id(&work));
	std::fs::write(admin.join("HEAD"), "ref: refs/heads/a\n").unwrap();

	// Oracle: stock git moves it (to a throwaway path, then back), confirming git does not resolve the chain.
	let probe = base.join("probe");
	assert!(
		git_ok(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"move",
			from.to_str().unwrap(),
			probe.to_str().unwrap(),
		]),
		"stock git moves a cyclic-HEAD worktree",
	);
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"move",
		probe.to_str().unwrap(),
		from.to_str().unwrap(),
	]);

	let to = base.join("to");
	let outcome = relocate(&req(&work, &from, &to, None))
		.await
		.expect("relocate moves the cyclic-HEAD worktree");
	assert_eq!(
		outcome,
		RelocateOutcome::Relocated {
			from: from.clone(),
			to: to.clone(),
		}
	);
	assert!(!from.exists(), "old checkout path gone");
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_accepts_a_dot_segment_source_alias() {
	// A caller (or the CLI) may pass an absolute dot-segment alias for the source — `<from>/sub/..`, which
	// resolves to `<from>`. Inspection accepts it by filesystem identity, but `rename` rejects a
	// `..`-terminated source with EINVAL, so relocate must canonicalize the source before renaming it. Stock
	// git and this crate's removal path accept the same alias.
	let base = unique_tmp("relocate-source-alias");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
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
	std::fs::create_dir(from.join("sub")).unwrap();

	// `<from>/sub/..` — an absolute alias of `from` ending in `..`.
	let alias = from.join("sub").join("..");
	let to = base.join("to");
	let outcome = relocate(&req(&work, &alias, &to, None))
		.await
		.expect("relocate resolves the dot-segment source and completes");
	assert_eq!(
		outcome,
		RelocateOutcome::Relocated {
			from: alias.clone(),
			to: to.clone(),
		}
	);
	assert!(!from.exists(), "the real source moved");
	assert_eq!(g(&["-C", to.to_str().unwrap(), "rev-parse", "HEAD"]), head);
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_matches_a_symbolic_alias_expected_branch_unpeeled() {
	// When HEAD directly names a symbolic alias and the pinned `expected_branch` is that same alias, structural
	// mode compares both **unpeeled** — so the pin matches and the move proceeds. Peeling only the expected
	// side (to the alias's target) would falsely report a branch mismatch.
	let base = unique_tmp("relocate-alias-branch");
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

	// A symbolic alias `refs/heads/alias -> refs/heads/feature`, with the worktree HEAD naming the alias.
	std::fs::write(
		work.join(".git/refs/heads/alias"),
		"ref: refs/heads/feature\n",
	)
	.unwrap();
	let admin = work.join(".git").join("worktrees").join(admin_id(&work));
	std::fs::write(admin.join("HEAD"), "ref: refs/heads/alias\n").unwrap();

	let to = base.join("to");
	relocate(&req(&work, &from, &to, Some("alias")))
		.await
		.expect("the unpeeled alias pin matches and moves");
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_matches_the_expected_branch_structurally() {
	// The move validates its pinned `expected_branch` against the worktree's HEAD branch read *structurally*
	// (no chain resolution): a mismatch is refused as an identity conflict; the exact branch moves.
	let base = unique_tmp("relocate-expected-branch");
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

	// Wrong branch → refused, source untouched.
	let to = base.join("to");
	let err = relocate(&req(&work, &from, &to, Some("other")))
		.await
		.expect_err("a pinned-branch mismatch is refused");
	assert!(
		matches!(
			err,
			RelocateError::Refused(WorktreeClassification::IdentityConflict { .. })
		),
		"got {err:?}"
	);
	assert!(from.exists() && !to.exists(), "the worktree stays put");

	// Correct branch → moves.
	relocate(&req(&work, &from, &to, Some("feature")))
		.await
		.expect("the exact expected branch moves");
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_refuses_a_destination_inside_a_prunable_admin() {
	// A destination beneath a *different* prunable admin directory (its checkout gone, so its `gitdir` names
	// the missing checkout, not `to`) is prune-unsafe: a later `git worktree prune` would recursively delete
	// the admin and the just-moved checkout inside it. Refuse it — even though no registration names `to`.
	let base = unique_tmp("relocate-into-prunable-admin");
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
	// A second worktree, then delete its checkout so its admin is prunable (retained, checkout missing).
	let victim = base.join("victim");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"vic",
		victim.to_str().unwrap(),
	]);
	std::fs::remove_dir_all(&victim).unwrap();
	let victim_admin = work.join(".git").join("worktrees").join("victim");
	assert!(victim_admin.is_dir(), "prunable admin retained");

	// `to` inside the prunable admin.
	let to = victim_admin.join("moved");
	let err = relocate(&req(&work, &from, &to, None))
		.await
		.expect_err("a destination inside a prunable admin is refused");
	assert!(
		matches!(err, RelocateError::DestinationInsideRegistration { .. }),
		"got {err:?}"
	);
	assert!(from.exists() && !to.exists(), "the worktree stays put");
}

#[tokio::test]
async fn relocate_moves_a_detached_head_with_trailing_content() {
	// `git worktree move`'s detached-HEAD grammar is lenient: a leading object-id of the repo's width with any
	// trailing content is accepted (verified against stock git), unlike the exact id the force-removal gate
	// requires. relocate reads HEAD with the move grammar, so it matches.
	let base = unique_tmp("relocate-detached-trailing");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	let head = commit_file(&work, "a.txt", "1\n", "init");
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

	// A 40-hex object id (SHA-1 width) followed by trailing text.
	let admin = work.join(".git").join("worktrees").join("from");
	std::fs::write(admin.join("HEAD"), format!("{head} trailing\n")).unwrap();

	// Oracle: stock git moves it (to a throwaway path, then back).
	let probe = base.join("probe");
	assert!(
		git_ok(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"move",
			from.to_str().unwrap(),
			probe.to_str().unwrap(),
		]),
		"stock git moves a trailing-content detached HEAD",
	);
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"move",
		probe.to_str().unwrap(),
		from.to_str().unwrap(),
	]);

	let to = base.join("to");
	relocate(&req(&work, &from, &to, None))
		.await
		.expect("relocate moves the lenient detached-HEAD worktree");
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}

#[tokio::test]
async fn relocate_reports_a_linked_worktree_checkout_occupant() {
	// When the destination is itself an existing linked-worktree checkout, the occupancy error distinguishes
	// it (`LinkedWorktreeCheckout`) from unrelated files, so structured callers can tell a worktree collision
	// from a stray directory.
	let base = unique_tmp("relocate-occupied-by-worktree");
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
	// A second, live worktree occupying the destination.
	let occupant = base.join("occupant");
	git(&[
		"-C",
		work.to_str().unwrap(),
		"worktree",
		"add",
		"-b",
		"other",
		occupant.to_str().unwrap(),
	]);

	let err = relocate(&req(&work, &from, &occupant, None))
		.await
		.expect_err("an occupied destination is refused");
	assert!(
		matches!(
			err,
			RelocateError::DestinationOccupied {
				kind: DestinationKind::LinkedWorktreeCheckout,
				..
			}
		),
		"got {err:?}"
	);
	assert!(from.exists(), "the source is untouched");
}

#[tokio::test]
async fn relocate_moves_with_a_cyclic_symbolic_branch_pin() {
	// The pinned `expected_branch` is matched by unpeeled name only; its ref chain is never resolved. So a pin
	// naming a cyclic symref that HEAD also names still moves — where resolving the pin would fail the move
	// git allows.
	let base = unique_tmp("relocate-cyclic-pin");
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

	// A self-cyclic symref `refs/heads/alias -> refs/heads/alias`, with the worktree HEAD naming it.
	std::fs::write(
		work.join(".git/refs/heads/alias"),
		"ref: refs/heads/alias\n",
	)
	.unwrap();
	let admin = work.join(".git").join("worktrees").join("from");
	std::fs::write(admin.join("HEAD"), "ref: refs/heads/alias\n").unwrap();

	let to = base.join("to");
	relocate(&req(&work, &from, &to, Some("alias")))
		.await
		.expect("the unpeeled pin matches without resolving the cyclic chain");
	assert!(
		git_listed_paths(&work).contains(&canonical(&to).to_string_lossy().into_owned()),
		"new path listed",
	);
}
