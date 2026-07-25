//! Read-only inspection + classification, oracle-checked against stock `git`, over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;

use common::*;
use gitana_linked_worktree::{
	BranchName, DestinationKind, HeadKind, IdentityConflict, Registration, RequestedBranch,
	WorktreeClassification, WorktreeObjectId, WorktreeQuery, classify, inspect, status,
};

/// A pure-inspection query (no expected branch), for classifying externally-created worktrees.
fn query_no_branch(
	repo: gitana_linked_worktree::RepositoryId,
	dest: &std::path::Path,
) -> WorktreeQuery {
	WorktreeQuery {
		repo,
		destination: dest.to_path_buf(),
		expected_branch: None,
		start: None,
		with_status: false,
		resolve_head: true,
	}
}

fn query(
	repo: gitana_linked_worktree::RepositoryId,
	dest: &std::path::Path,
	branch: &str,
) -> WorktreeQuery {
	// The destination is passed *raw* (not canonicalized): the library must not follow a symlink at the
	// destination itself, and it canonicalizes internally where identity comparison needs it.
	WorktreeQuery {
		repo,
		destination: dest.to_path_buf(),
		expected_branch: Some(BranchName::new(branch)),
		start: None,
		with_status: false,
		resolve_head: true,
	}
}

/// A query carrying a reconciliation `start` (the commit the caller intends the branch to sit at), so
/// inspection computes the ancestry relation `classify` needs.
fn query_with_start(
	repo: gitana_linked_worktree::RepositoryId,
	dest: &std::path::Path,
	branch: &str,
	start: WorktreeObjectId,
) -> WorktreeQuery {
	WorktreeQuery {
		repo,
		destination: dest.to_path_buf(),
		expected_branch: Some(BranchName::new(branch)),
		start: Some(start),
		with_status: false,
		resolve_head: true,
	}
}

#[tokio::test]
async fn absent_destination_is_safe_to_create() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("absent-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");

		let dest = base.join("wt"); // does not exist
		let insp = inspect(&query(rid_at(&work), &dest, "feature"))
			.await
			.unwrap();

		assert_eq!(insp.destination_kind, DestinationKind::Absent);
		assert_eq!(insp.registration, Registration::None);
		assert!(matches!(
			insp.requested_branch,
			RequestedBranch::Absent {
				checked_out_elsewhere: None
			}
		));
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::AbsentSafeToCreate
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_created_branch_without_a_worktree_is_interrupted_completable() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("interrupted-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let start_hex = commit_file(&work, "a.txt", "1\n", "init");
		git(&[
			"-C",
			work.to_str().unwrap(),
			"branch",
			"feature",
			&start_hex,
		]);

		let dest = base.join("wt");
		let start = WorktreeObjectId::parse(kind, &start_hex).unwrap();
		let insp = inspect(&query_with_start(rid_at(&work), &dest, "feature", start))
			.await
			.unwrap();
		assert!(matches!(
			insp.requested_branch,
			RequestedBranch::Exists { .. }
		));

		match classify(&insp) {
			WorktreeClassification::InterruptedCompletable { branch_object } => {
				assert_eq!(branch_object.to_hex(), start_hex);
			}
			other => panic!("{fmt}: expected InterruptedCompletable, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_exact_matching_worktree_is_complete_and_idempotent() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("complete-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();

		let start = WorktreeObjectId::parse(kind, &feat).unwrap();
		let insp = inspect(&query_with_start(
			rid_at(&work),
			&wt,
			"feature",
			start.clone(),
		))
		.await
		.unwrap();
		assert!(matches!(insp.registration, Registration::Present { .. }));
		assert_eq!(
			insp.cross_pointers,
			gitana_linked_worktree::CrossPointerHealth::Consistent
		);

		assert!(matches!(
			classify(&insp),
			WorktreeClassification::CompleteIdempotent { .. }
		));
		// Re-inspection is stable (idempotent).
		let again = inspect(&query_with_start(rid_at(&work), &wt, "feature", start))
			.await
			.unwrap();
		assert_eq!(insp, again);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_registration_whose_checkout_is_gone_is_partial_registered() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("partial-reg-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		std::fs::remove_dir_all(&wt).unwrap(); // checkout gone, registration retained

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(matches!(
			insp.registration,
			Registration::PresentCheckoutMissing { .. }
		));
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::PartialRegistered { .. }
		));
		// git agrees the registration is prunable.
		let listing = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			listing.contains("prunable"),
			"git should mark it prunable: {listing}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn disagreeing_cross_pointers_are_an_identity_conflict_and_not_repaired() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("xpointer-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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

		// Corrupt the admin's back-pointer so it no longer names the checkout.
		let admin_gitdir = work.join(".git/worktrees/wt/gitdir");
		let bogus = "/nonexistent/elsewhere/.git\n";
		std::fs::write(&admin_gitdir, bogus).unwrap();

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(matches!(
			insp.cross_pointers,
			gitana_linked_worktree::CrossPointerHealth::Inconsistent { .. }
		));
		assert_eq!(
			insp.identity_conflict,
			Some(IdentityConflict::CrossPointerDisagree)
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::CrossPointerDisagree
			}
		));
		// Inspection must not have rewritten the corrupt pointer.
		assert_eq!(std::fs::read_to_string(&admin_gitdir).unwrap(), bogus);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_branch_checked_out_elsewhere_is_a_branch_use_conflict() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("branch-use-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt1 = base.join("wt1");
		let w = work.to_str().unwrap();
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt1.to_str().unwrap(),
		]);

		let wt2 = base.join("wt2"); // different, absent destination
		let insp = inspect(&query(rid_at(&work), &wt2, "feature"))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert!(canonical(&other_checkout) == canonical(&wt1));
			}
			other => panic!("{fmt}: expected BranchUseConflict, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn non_directory_and_unrelated_destinations_are_destination_conflicts() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("dest-conflict-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let rid = || rid_at(&work);

		// A plain file.
		let file = base.join("as-file");
		std::fs::write(&file, b"x").unwrap();
		let insp = inspect(&query(rid(), &file, "feature")).await.unwrap();
		assert_eq!(insp.destination_kind, DestinationKind::OtherFsObject);

		// A non-empty directory with no `.git`.
		let dir = base.join("unrelated");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("stuff"), b"x").unwrap();
		let insp = inspect(&query(rid(), &dir, "feature")).await.unwrap();
		assert_eq!(insp.destination_kind, DestinationKind::UnrelatedContent);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::DestinationConflict { .. }
		));

		// A symlink at the destination is a non-directory — never followed.
		let link = base.join("as-link");
		symlink(&work, &link).unwrap();
		let insp = inspect(&query(rid(), &link, "feature")).await.unwrap();
		assert_eq!(insp.destination_kind, DestinationKind::OtherFsObject);

		// A directory whose `.git` is a *symlink* is unrelated content — the symlink is not followed.
		let sneaky = base.join("sneaky");
		std::fs::create_dir_all(&sneaky).unwrap();
		symlink(work.join(".git"), sneaky.join(".git")).unwrap();
		let insp = inspect(&query(rid(), &sneaky, "feature")).await.unwrap();
		assert_eq!(insp.destination_kind, DestinationKind::UnrelatedContent);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_checkout_replaced_by_an_empty_dir_is_partial_registered() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("replaced-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		// Delete the checkout and recreate the path as an empty directory — git marks it prunable.
		std::fs::remove_dir_all(&wt).unwrap();
		std::fs::create_dir_all(&wt).unwrap();

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(
			matches!(
				insp.registration,
				Registration::PresentCheckoutMissing { .. }
			),
			"{fmt}: a checkout whose .git is gone is a missing checkout, not Present"
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::PartialRegistered { .. }
		));
		let listing = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			listing.contains("prunable"),
			"git agrees it is prunable: {listing}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_destination_owned_by_another_repository_is_an_identity_conflict() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("foreign-{fmt}"));
		// Two independent repositories; a worktree of the *other* repo is inspected against ours.
		let ours = base.join("ours");
		let other = base.join("other");
		init_repo(&ours, fmt);
		commit_file(&ours, "a.txt", "1\n", "init");
		init_repo(&other, fmt);
		commit_file(&other, "a.txt", "1\n", "init");
		let foreign_wt = base.join("foreign-wt");
		git(&[
			"-C",
			other.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			foreign_wt.to_str().unwrap(),
		]);

		// Inspect the other repo's worktree against *our* repository identity.
		let insp = inspect(&query(rid_at(&ours), &foreign_wt, "feature"))
			.await
			.unwrap();
		match &insp.identity_conflict {
			Some(IdentityConflict::DestinationBelongsToOtherWorktree { .. }) => {}
			other => panic!("{fmt}: expected DestinationBelongsToOtherWorktree, got {other:?}"),
		}
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::DestinationBelongsToOtherWorktree { .. }
			}
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlink_destination_to_a_checkout_is_a_conflict_not_a_worktree() {
	// A destination that is itself a *symlink* (even to a real registered checkout) is `OtherFsObject` — a
	// destination conflict, never a registration. The alias must not be followed to read the target's
	// `.git`/HEAD/lock (which could return a lock reason as `ProtectedWithReason`), and `status` must refuse.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-dest-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"SECRET",
			wt.to_str().unwrap(),
		]);
		let alias = base.join("alias");
		symlink(&wt, &alias).unwrap();

		let insp = inspect(&query(rid_at(&work), &alias, "feature"))
			.await
			.unwrap();
		assert_eq!(insp.destination_kind, DestinationKind::OtherFsObject);
		assert!(
			!matches!(insp.registration, Registration::Present { .. }),
			"{fmt}: a symlink alias is not a registration"
		);
		assert_ne!(
			insp.lock,
			gitana_linked_worktree::LockState::Locked {
				reason: Some("SECRET".to_owned())
			},
			"{fmt}: the alias target's lock must not be read"
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::DestinationConflict {
				kind: DestinationKind::OtherFsObject
			}
		));
		// Status must refuse the symlink alias.
		assert!(
			status(&rid_at(&work), &alias).await.is_err(),
			"{fmt}: status must refuse a symlink destination"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_requested_legacy_symlink_symref_branch_exists() {
	// A requested branch that is a legacy *symlink* symref (`refs/heads/alias -> refs/heads/feature`) is
	// symbolic to git and resolves — its object must be read through the terminal ref, not by following the
	// filesystem symlink, so it is reported `Exists`, not `Absent`.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("req-symlink-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature"]);
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();
		symlink("refs/heads/feature", work.join(".git/refs/heads/alias")).unwrap();
		// Oracle: git resolves the symlink symref to feature's object.
		assert_eq!(
			git(&["-C", w, "rev-parse", "refs/heads/alias"]).trim(),
			feat,
			"{fmt}: git resolves the symlink symref"
		);

		let insp = inspect(&query(rid_at(&work), &base.join("wt"), "alias"))
			.await
			.unwrap();
		match &insp.requested_branch {
			RequestedBranch::Exists { object, .. } => assert_eq!(object.to_hex(), feat),
			other => panic!("{fmt}: a symlink-symref branch must exist, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_start_of_the_wrong_hash_kind_is_rejected() {
	// A `start` whose hash format differs from the repository cannot belong to (or be created in) it —
	// inspection must reject it up front, not silently drop the ancestry and classify AbsentSafeToCreate.
	let base = unique_tmp("start-kind");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	let sha256_start =
		WorktreeObjectId::parse(gitana_object::HashKind::Sha256, &"a".repeat(64)).unwrap();
	let q = query_with_start(rid_at(&work), &base.join("wt"), "feature", sha256_start);
	assert!(
		inspect(&q).await.is_err(),
		"a sha256 start against a sha1 repository must be rejected"
	);
	let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn a_symlinked_external_admin_is_not_a_registration_and_leaks_no_lock() {
	// A `worktrees/<name>` that is a *symlink* to an external directory is followed for branch-use (the ref
	// name only), but must NOT be a *registration* — reading a registration's full per-worktree state
	// (HEAD/lock/index) through such a symlink would cross the ambient-read boundary and could expose a
	// lock file's contents as the public reason. Registration requires strict physical ownership.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-ext-admin-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Relocate the admin OUTSIDE worktrees/, symlink to it, and plant a secret lock reason.
		let admin = work.join(".git/worktrees/wt");
		let external = base.join("external-admin");
		std::fs::rename(&admin, &external).unwrap();
		symlink(&external, &admin).unwrap();
		std::fs::write(external.join("locked"), b"TOP SECRET").unwrap();

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(
			!matches!(insp.registration, Registration::Present { .. }),
			"{fmt}: a symlinked external admin is not a registration, got {:?}",
			insp.registration
		);
		assert_ne!(
			insp.lock,
			gitana_linked_worktree::LockState::Locked {
				reason: Some("TOP SECRET".to_owned())
			},
			"{fmt}: the external admin's lock contents must not be exposed"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_stray_main_gitdir_does_not_hide_a_primary_branch_conflict() {
	// The primary is on `feature`. A stray `gitdir` file in the main `.git` (which git ignores) must not be
	// read as the primary's checkout path in the branch-use scan — otherwise, if it named the destination,
	// the primary would be wrongly excluded and an occupied branch misclassified as InterruptedCompletable.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("stray-main-gitdir-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let start_hex = commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// Put the *primary* on `feature`.
		git(&["-C", w, "checkout", "-q", "-b", "feature"]);
		let wt2 = base.join("wt2");
		// A stray `gitdir` in the main `.git` naming the destination being inspected.
		std::fs::write(
			work.join(".git/gitdir"),
			format!("{}/.git\n", wt2.display()),
		)
		.unwrap();
		// Oracle: git refuses to check `feature` out again (it is on the primary).
		assert!(
			!git_ok(&["-C", w, "worktree", "add", wt2.to_str().unwrap(), "feature"]),
			"{fmt}: git refuses a second checkout of the primary's branch"
		);
		let _ = std::fs::remove_dir_all(&wt2); // clean the failed worktree attempt

		let start = WorktreeObjectId::parse(kind, &start_hex).unwrap();
		let insp = inspect(&query_with_start(rid_at(&work), &wt2, "feature", start))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert_eq!(canonical(&other_checkout), canonical(&work));
			}
			other => panic!("{fmt}: the primary must still occupy `feature`, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_commondir_mismatched_worktree_still_occupies_its_branch() {
	// git lists a linked worktree whose `commondir` is retargeted and still refuses another checkout of its
	// branch. So while such an admin is *not our registration* for its own destination (ownership guard), it
	// MUST still count in branch-use scanning — a new destination requesting its branch is a conflict.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("commondir-branchuse-{fmt}"));
		let ours = base.join("ours");
		let other = base.join("other");
		init_repo(&ours, fmt);
		commit_file(&ours, "a.txt", "1\n", "init");
		init_repo(&other, fmt);
		commit_file(&other, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git(&[
			"-C",
			ours.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feat",
			wt.to_str().unwrap(),
		]);
		// Retarget the admin's commondir at the *other* repository.
		std::fs::write(
			ours.join(".git/worktrees/wt/commondir"),
			format!("{}\n", other.join(".git").display()),
		)
		.unwrap();
		// Oracle: git still refuses a second checkout of `feat` from ours.
		assert!(
			!git_ok(&[
				"-C",
				ours.to_str().unwrap(),
				"worktree",
				"add",
				base.join("wt2").to_str().unwrap(),
				"feat"
			]),
			"{fmt}: git still refuses the second checkout"
		);

		// Inspecting a *new* destination for `feat` must report the branch-use conflict at wt.
		let insp = inspect(&query(rid_at(&ours), &base.join("wt3"), "feat"))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert_eq!(canonical(&other_checkout), canonical(&wt));
			}
			other => panic!("{fmt}: a commondir-mismatched worktree must still conflict, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlinked_admin_conflict_does_not_leak_its_gitdir() {
	// Branch-use follows a symlinked admin for the ref *name* only. It must NOT dereference that admin's
	// `gitdir` file (a crafted external admin could otherwise leak its contents as `other_checkout`). The
	// conflict is reported at the admin's own owned location instead.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-admin-leak-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feat",
			wt.to_str().unwrap(),
		]);
		// Relocate the admin out, symlink to it, and craft its gitdir to a "secret" path.
		let admin = work.join(".git/worktrees/wt");
		let external = base.join("external-admin");
		std::fs::rename(&admin, &external).unwrap();
		symlink(&external, &admin).unwrap();
		std::fs::write(external.join("gitdir"), b"/secret/leaked/path/.git\n").unwrap();

		let insp = inspect(&query(rid_at(&work), &base.join("wt3"), "feat"))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert!(
					!other_checkout.to_string_lossy().contains("leaked"),
					"{fmt}: the crafted gitdir must not leak into other_checkout: {other_checkout:?}"
				);
				assert_eq!(
					canonical(&other_checkout),
					canonical(&admin),
					"{fmt}: the conflict is reported at the admin's own location"
				);
			}
			other => panic!("{fmt}: expected a branch-use conflict, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlinked_admin_still_occupies_its_branch() {
	// git *follows* a symlinked admin dir under `worktrees/`, lists it, and refuses another checkout of its
	// branch. Branch-use scanning must follow it too (reading only the ref name), reporting the conflict —
	// not silently drop it.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-admin-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feat",
			wt.to_str().unwrap(),
		]);
		// Relocate the admin and leave a symlink in its place.
		let admin = work.join(".git/worktrees/wt");
		let real = work.join(".git/worktrees/real");
		std::fs::rename(&admin, &real).unwrap();
		symlink("real", &admin).unwrap();
		// Oracle: git still refuses a second checkout of `feat`.
		assert!(
			!git_ok(&[
				"-C",
				w,
				"worktree",
				"add",
				base.join("wt2").to_str().unwrap(),
				"feat"
			]),
			"{fmt}: git still refuses the second checkout through the symlinked admin"
		);

		let insp = inspect(&query(rid_at(&work), &base.join("wt3"), "feat"))
			.await
			.unwrap();
		assert!(
			matches!(
				classify(&insp),
				WorktreeClassification::BranchUseConflict { .. }
			),
			"{fmt}: a symlinked admin must still occupy its branch, got {:?}",
			classify(&insp)
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_malformed_head_behind_a_symlinked_admin_is_not_disclosed() {
	// Branch-use follows a symlinked admin and resolves its HEAD. A **malformed** HEAD behind that redirect
	// must NOT have its contents surfaced in the error: `resolve_ref_terminal` reports the HEAD *file path*,
	// never the parsed ref name. Same confused-deputy family as a symlinked lock marker — an admin symlinked
	// to an external directory would otherwise leak a line of a file there through the error message.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-head-leak-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feat",
			wt.to_str().unwrap(),
		]);

		// Relocate the admin outside `.git`, symlink it back, and plant a malformed HEAD carrying a secret.
		let admin = work.join(".git/worktrees/wt");
		let external = base.join("external-admin");
		std::fs::rename(&admin, &external).unwrap();
		symlink(&external, &admin).unwrap();
		std::fs::write(external.join("HEAD"), b"ref: TOP_SECRET_LEAKED\n").unwrap();

		// Branch-use over the symlinked admin hits the malformed HEAD. Whether it surfaces as an `Err` or is
		// absorbed, the secret must never appear in any rendered diagnostic.
		let result = inspect(&query(rid_at(&work), &base.join("dest"), "feat")).await;
		let rendered = match &result {
			Err(e) => format!("{e}"),
			Ok(insp) => format!("{insp:?}"),
		};
		assert!(
			!rendered.contains("TOP_SECRET_LEAKED"),
			"{fmt}: a malformed HEAD behind a symlinked admin leaked its contents: {rendered}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_invalid_requested_branch_is_not_blamed_on_head() {
	// A malformed *requested branch* (a caller argument) must surface as `InvalidRequestedBranch`, never as
	// a `MalformedPointer` blaming the repository's healthy `HEAD` — the branch is caller-supplied, so
	// resolving it is a distinct job from resolving a worktree's HEAD chain.
	use gitana_linked_worktree::LinkedWorktreeError;
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("bad-req-branch-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		// A branch name with a space is not a valid ref (`refs/heads/bad name` fails check-ref-format), yet
		// HEAD is perfectly healthy.
		let err = inspect(&query(rid_at(&work), &base.join("dest"), "bad name"))
			.await
			.expect_err(&format!("{fmt}: an invalid requested branch must error"));
		assert!(
			matches!(&err, LinkedWorktreeError::InvalidRequestedBranch(name) if name == "bad name"),
			"{fmt}: expected InvalidRequestedBranch(\"bad name\"), got {err:?}"
		);
		let rendered = format!("{err}");
		assert!(
			!rendered.contains("HEAD"),
			"{fmt}: a bad requested branch must not be blamed on HEAD: {rendered}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_unreadable_head_target_behind_a_symlinked_admin_is_not_disclosed() {
	// The no-disclosure contract must hold on the *I/O* path too, not only the malformed-refname path: a
	// HEAD target that is a syntactically valid ref name but cannot be opened (a component over NAME_MAX →
	// ENAMETOOLONG) must not surface `base.join(name)` — which, behind a symlinked admin, embeds a line read
	// from behind the redirect.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("head-io-leak-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feat",
			wt.to_str().unwrap(),
		]);

		let admin = work.join(".git/worktrees/wt");
		let external = base.join("external-admin");
		std::fs::rename(&admin, &external).unwrap();
		symlink(&external, &admin).unwrap();
		// Valid ref chars, but a component far over NAME_MAX so the ref-file open fails with ENAMETOOLONG.
		let secret = format!("SECRET_{}", "z".repeat(300));
		std::fs::write(external.join("HEAD"), format!("ref: refs/heads/{secret}\n")).unwrap();

		let result = inspect(&query(rid_at(&work), &base.join("dest"), "feat")).await;
		let rendered = match &result {
			Err(e) => format!("{e}"),
			Ok(insp) => format!("{insp:?}"),
		};
		assert!(
			!rendered.contains("SECRET_"),
			"{fmt}: an unreadable HEAD target behind a symlinked admin leaked via the I/O error: {rendered}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_valid_branch_with_a_corrupt_symref_is_repo_corruption_not_caller_error() {
	// A requested branch that is a *valid name* but whose on-disk symref chain is broken is REPOSITORY
	// corruption, not a bad caller argument: it must surface as `MalformedPointer` (kind `Ref`) naming the
	// branch ref file, never `InvalidRequestedBranch` (which would wrongly accuse the caller) and never the
	// corrupt target's contents.
	use gitana_linked_worktree::{LinkedWorktreeError, PointerKind};
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("corrupt-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// `alias` is a real, validly-named branch that is a symbolic ref — but its target is corrupt (an
		// invalid ref name). The caller's `alias` is fine; the fault is on disk.
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			"refs/heads/main",
		]);
		// The target carries a space — an *invalid* ref name (check-ref-format rejects it), so the chain is
		// genuinely broken rather than merely pointing at an absent-but-valid ref that would resolve cleanly.
		std::fs::write(
			work.join(".git/refs/heads/alias"),
			b"ref: refs/heads/SECRET LEAK\n",
		)
		.unwrap();

		let err = inspect(&query(rid_at(&work), &base.join("dest"), "alias"))
			.await
			.expect_err(&format!("{fmt}: a corrupt symref chain must error"));
		assert!(
			matches!(
				&err,
				LinkedWorktreeError::MalformedPointer {
					kind: PointerKind::Ref,
					..
				}
			),
			"{fmt}: expected MalformedPointer(Ref) for on-disk corruption, got {err:?}"
		);
		let rendered = format!("{err}");
		assert!(
			!rendered.contains("SECRET LEAK"),
			"{fmt}: the corrupt symref target's contents leaked: {rendered}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_corrupt_requested_branch_that_is_checked_out_is_classified_by_the_branch() {
	// Compound case: the requested branch is a corrupt symref AND is currently checked out somewhere. The
	// occupancy scan would peel the occupying worktree's HEAD onto the same corrupt chain and report a
	// `Head` malformation — but the requester asked about the *branch*, so classification must be `Ref`
	// rooted at the branch, resolved before the scan runs. (Neither is a leak; this pins the classification.)
	use gitana_linked_worktree::{LinkedWorktreeError, PointerKind};
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("corrupt-checked-out-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// `alias` is a symbolic ref; check a worktree out on it, then corrupt alias's target (space = invalid).
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			"refs/heads/main",
		]);
		let wt = base.join("wt");
		git(&["-C", w, "worktree", "add", wt.to_str().unwrap(), "alias"]);
		let admin_name = std::fs::read_dir(work.join(".git/worktrees"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.file_name();
		std::fs::write(
			work.join(".git/worktrees").join(&admin_name).join("HEAD"),
			b"ref: refs/heads/alias\n",
		)
		.unwrap();
		std::fs::write(
			work.join(".git/refs/heads/alias"),
			b"ref: refs/heads/BAD NAME\n",
		)
		.unwrap();

		let err = inspect(&query(rid_at(&work), &base.join("dest"), "alias"))
			.await
			.expect_err(&format!(
				"{fmt}: a corrupt checked-out requested branch must error"
			));
		assert!(
			matches!(
				&err,
				LinkedWorktreeError::MalformedPointer {
					kind: PointerKind::Ref,
					..
				}
			),
			"{fmt}: expected the branch-rooted Ref classification, got {err:?}"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_admin_whose_commondir_points_elsewhere_is_not_our_registration() {
	// git treats `<admin>/commondir` as authoritative: it names the shared repository the worktree belongs
	// to. An admin under *our* `worktrees/` whose `commondir` is retargeted at another repository is that
	// repository's worktree, not ours — so it must NOT be accepted as our registration and force-opened
	// against our object store (which could report a bogus consistent/complete state or clean status).
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("commondir-{fmt}"));
		let ours = base.join("ours");
		let other = base.join("other");
		init_repo(&ours, fmt);
		commit_file(&ours, "a.txt", "1\n", "init");
		init_repo(&other, fmt);
		commit_file(&other, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git(&[
			"-C",
			ours.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Retarget the admin's `commondir` at the *other* repository's git dir.
		std::fs::write(
			ours.join(".git/worktrees/wt/commondir"),
			format!("{}\n", other.join(".git").display()),
		)
		.unwrap();
		// Oracle: git in the checkout now reads the *other* repository as its common dir.
		let common = git(&["-C", wt.to_str().unwrap(), "rev-parse", "--git-common-dir"]);
		assert_eq!(
			canonical(std::path::Path::new(common.trim())),
			canonical(&other.join(".git")),
			"{fmt}: git resolves the retargeted common dir"
		);

		// Anchored on *ours*, the admin is foreign (its `commondir` names another repo). It must be reported
		// as an identity conflict, its HEAD never read (no fabricated facts) — not our registration.
		let insp = inspect(&query(rid_at(&ours), &wt, "feature"))
			.await
			.unwrap();
		assert_eq!(
			insp.registration,
			Registration::None,
			"{fmt}: an admin owned by another repository is not our registration"
		);
		assert!(
			matches!(
				insp.identity_conflict,
				Some(IdentityConflict::DestinationBelongsToOtherWorktree { .. })
			),
			"{fmt}: a foreign-owned checkout is an identity conflict, got {:?}",
			insp.identity_conflict
		);
		assert!(
			insp.head.is_none(),
			"{fmt}: the foreign admin's HEAD must not be dereferenced"
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::DestinationBelongsToOtherWorktree { .. }
			}
		));
		// Status must refuse rather than open our object store against the wrong worktree.
		assert!(
			status(&rid_at(&ours), &canonical(&wt)).await.is_err(),
			"{fmt}: status of a foreign-owned checkout must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symbolic_branch_ref_resolves_instead_of_erroring() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let main = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();
		// `refs/heads/alias` is a *symbolic* ref to the default branch — `resolve_symbolic` follows it.
		let def = git(&["-C", w, "rev-parse", "--abbrev-ref", "HEAD"])
			.trim()
			.to_owned();
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			&format!("refs/heads/{def}"),
		]);

		let dest = base.join("wt");
		let insp = inspect(&query(rid_at(&work), &dest, "alias"))
			.await
			.unwrap();
		match &insp.requested_branch {
			RequestedBranch::Exists { object, .. } => assert_eq!(object.to_hex(), main),
			other => panic!("{fmt}: symbolic branch should resolve, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_branch_forced_into_two_worktrees_still_conflicts_from_the_first() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("dup-force-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt1 = base.join("wt1");
		let wt2 = base.join("wt2");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt1.to_str().unwrap(),
		]);
		// `--force` checks the same branch out in a second worktree.
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"--force",
			wt2.to_str().unwrap(),
			"feature",
		]);

		// Inspecting wt1 must still report the *other* checkout (wt2), not "no conflict".
		let insp = inspect(&query(rid_at(&work), &wt1, "feature"))
			.await
			.unwrap();
		match &insp.requested_branch {
			RequestedBranch::Exists {
				checked_out_elsewhere: Some(other),
				..
			} => assert_eq!(canonical(other), canonical(&wt2)),
			other => panic!("{fmt}: expected the other checkout, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_unsupported_object_format_maps_to_the_documented_variant() {
	// One format is enough; this is about the error mapping, not object contents.
	let base = unique_tmp("unsupported-format");
	let work = base.join("repo");
	init_repo(&work, "sha1");
	commit_file(&work, "a.txt", "1\n", "init");
	// Declare an object format gitana does not support.
	let config = work.join(".git/config");
	let mut text = std::fs::read_to_string(&config).unwrap();
	text.push_str("[extensions]\n\tobjectformat = sha999\n");
	std::fs::write(&config, text).unwrap();

	let err = inspect(&query(rid_at(&work), &base.join("wt"), "feature"))
		.await
		.unwrap_err();
	assert!(
		matches!(
			err,
			gitana_linked_worktree::LinkedWorktreeError::UnsupportedObjectFormat(_)
		),
		"expected UnsupportedObjectFormat, got {err:?}"
	);
	let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn duplicate_registrations_for_one_destination_are_an_identity_conflict() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("dup-reg-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		// Corrupt the admin registry: a *second* admin entry pointing at the same checkout. It carries the
		// same `gitdir` back-pointer *and* the same `commondir` (a genuine duplicate registration of this
		// repository, not a foreign one), so it must be counted as ours.
		let dup = work.join(".git/worktrees/wt-dup");
		std::fs::create_dir_all(&dup).unwrap();
		std::fs::copy(work.join(".git/worktrees/wt/gitdir"), dup.join("gitdir")).unwrap();
		std::fs::copy(
			work.join(".git/worktrees/wt/commondir"),
			dup.join("commondir"),
		)
		.unwrap();

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(
			matches!(
				insp.identity_conflict,
				Some(IdentityConflict::DuplicateRegistration { .. })
			),
			"{fmt}: two registrations for one destination must conflict, got {:?}",
			insp.identity_conflict
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::DuplicateRegistration { .. }
			}
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_requested_alias_does_not_conflict_with_a_worktree_on_its_target() {
	// git allows `worktree add ... alias` even when `feature` (alias's target) is checked out elsewhere:
	// the shared-symref test peels worktree HEADs but keeps the requested ref name unpeeled.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("alias-nocflct-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt1 = base.join("wt1");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt1.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			"refs/heads/feature",
		]);

		// Requesting `alias` at a new destination: `feature` is checked out at wt1, but the request is for
		// `alias` — no branch-use conflict, matching git.
		let insp = inspect(&query(rid_at(&work), &base.join("wt2"), "alias"))
			.await
			.unwrap();
		match &insp.requested_branch {
			RequestedBranch::Exists {
				checked_out_elsewhere,
				..
			} => {
				assert_eq!(
					*checked_out_elsewhere, None,
					"{fmt}: alias must not conflict with feature"
				)
			}
			other => panic!("{fmt}: alias should exist, got {other:?}"),
		}
		assert!(!matches!(
			classify(&insp),
			WorktreeClassification::BranchUseConflict { .. }
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_unborn_branch_checked_out_elsewhere_still_conflicts() {
	// `git worktree add --orphan <branch>` checks out an *unborn* branch: HEAD points at
	// `refs/heads/<branch>` but that ref does not yet exist. The branch is nonetheless occupied, and git
	// refuses to check it out a second time. Ref resolution returns `None` (unborn), so the branch-use
	// scan of worktree HEADs — not ref existence — is what must catch the conflict.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("orphan-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let orphan_wt = base.join("orphan");
		// `--orphan` requires a reasonably modern git; if unavailable, skip rather than fail.
		if !git_ok(&[
			"-C",
			w,
			"worktree",
			"add",
			"--orphan",
			"-b",
			"fresh",
			orphan_wt.to_str().unwrap(),
		]) {
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}
		// The ref really is unborn: git reports it as an unborn branch, not a resolvable object.
		assert!(
			!git_ok(&["-C", w, "rev-parse", "--verify", "refs/heads/fresh"]),
			"{fmt}: `fresh` must be unborn"
		);

		let wt2 = base.join("wt2"); // a second, absent destination requesting the same branch
		let insp = inspect(&query(rid_at(&work), &wt2, "fresh")).await.unwrap();
		match &insp.requested_branch {
			RequestedBranch::Absent {
				checked_out_elsewhere: Some(other),
			} => assert_eq!(canonical(other), canonical(&orphan_wt)),
			other => {
				panic!("{fmt}: an occupied unborn branch must report the other checkout, got {other:?}")
			}
		}
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert_eq!(canonical(&other_checkout), canonical(&orphan_wt));
			}
			other => panic!("{fmt}: expected BranchUseConflict, got {other:?}"),
		}
		// Oracle: git itself refuses to check the same unborn branch out again.
		assert!(
			!git_ok(&["-C", w, "worktree", "add", wt2.to_str().unwrap(), "fresh"]),
			"{fmt}: git must refuse the second checkout of an unborn branch"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_case_variant_destination_matches_on_a_case_insensitive_filesystem() {
	// On a case-insensitive filesystem (default macOS/Windows) `/base/WT` and `/base/wt` are the same
	// directory, which git accepts interchangeably. Identity comparison must be by filesystem identity, not
	// canonical spelling, so a case-variant query still recognizes the registered worktree rather than
	// reporting a phantom missing registration. Skipped where the filesystem is case-sensitive.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("case-{fmt}"));
		// Probe: does `<base>/CASEPROBE` also resolve as `<base>/caseprobe`?
		std::fs::create_dir(base.join("CASEPROBE")).unwrap();
		let case_insensitive = base.join("caseprobe").exists();
		std::fs::remove_dir(base.join("CASEPROBE")).unwrap();
		if !case_insensitive {
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}

		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();

		// Query the *same* worktree by a case-variant path; it must be recognized as the registered checkout.
		let variant = base.join("WT");
		let start = WorktreeObjectId::parse(kind, &feat).unwrap();
		let insp = inspect(&query_with_start(rid_at(&work), &variant, "feature", start))
			.await
			.unwrap();
		assert!(
			matches!(insp.registration, Registration::Present { .. }),
			"{fmt}: a case-variant path must match the registered worktree, got {:?}",
			insp.registration
		);
		assert_eq!(insp.identity_conflict, None);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::CompleteIdempotent { .. }
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_deleted_registered_checkout_queried_case_variant_is_partial_registered() {
	// On a case-insensitive filesystem, a registered checkout `CaSeWt` whose directory was deleted, then
	// queried by a case-variant path `casewt`, is the *same* (missing) registered worktree git recognizes.
	// Identity comparison must fold case even for the missing paths, so it classifies as PartialRegistered,
	// not a phantom BranchUseConflict. Skipped where the filesystem is case-sensitive.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("case-missing-{fmt}"));
		std::fs::create_dir(base.join("CASEPROBE")).unwrap();
		let case_insensitive = base.join("caseprobe").exists();
		std::fs::remove_dir(base.join("CASEPROBE")).unwrap();
		if !case_insensitive {
			let _ = std::fs::remove_dir_all(&base);
			continue;
		}

		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let cased = base.join("CaSeWt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			cased.to_str().unwrap(),
		]);
		// Delete the checkout (registration retained), and query by a differently-cased path.
		std::fs::remove_dir_all(&cased).unwrap();
		let variant = base.join("casewt");

		let insp = inspect(&query(rid_at(&work), &variant, "feature"))
			.await
			.unwrap();
		assert!(
			matches!(
				insp.registration,
				Registration::PresentCheckoutMissing { .. }
			),
			"{fmt}: a case-variant path must still match the missing registration, got {:?}",
			insp.registration
		);
		assert!(matches!(
			classify(&insp),
			WorktreeClassification::PartialRegistered { .. }
		));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlink_direct_ref_branch_is_its_own_occupied_branch() {
	// A ref file that is a *symlink to a sibling* (`refs/heads/alias -> feature`, target NOT `refs/`-
	// prefixed) is a **direct** ref in git — `alias` is its own branch (git follows the link to read the
	// object), distinct from `feature`. A worktree on `alias` therefore occupies `alias`, and a second
	// `worktree add ... alias` is refused. (Contrast the `ref:`-file / `refs/`-symlink symref, which is an
	// alias of its target.)
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-directref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature"]);
		// A *symlink* ref file whose target is a bare sibling name — a legacy direct ref, not a symref.
		symlink("feature", work.join(".git/refs/heads/alias")).unwrap();
		// git agrees `alias` is a direct ref, not symbolic.
		assert!(
			!git_ok(&["-C", w, "symbolic-ref", "refs/heads/alias"]),
			"{fmt}: a sibling-target ref symlink is a direct ref, not symbolic"
		);
		let wt1 = base.join("wt1");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-q",
			wt1.to_str().unwrap(),
			"alias",
		]);

		// Requesting `alias` at a *new* destination must see the occupied branch (wt1).
		let insp = inspect(&query(rid_at(&work), &base.join("wt2"), "alias"))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::BranchUseConflict { other_checkout } => {
				assert_eq!(canonical(&other_checkout), canonical(&wt1));
			}
			other => panic!("{fmt}: a symlink-direct-ref branch must conflict, got {other:?}"),
		}
		// Oracle: git refuses the second checkout of `alias`, but `feature` is still free.
		assert!(
			!git_ok(&[
				"-C",
				w,
				"worktree",
				"add",
				base.join("wt3").to_str().unwrap(),
				"alias"
			]),
			"{fmt}: git must refuse a second `alias` checkout"
		);
		let insp_feature = inspect(&query(rid_at(&work), &base.join("wt4"), "feature"))
			.await
			.unwrap();
		assert!(
			!matches!(
				classify(&insp_feature),
				WorktreeClassification::BranchUseConflict { .. }
			),
			"{fmt}: `feature` is a distinct, free branch"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_head_symlink_to_a_non_ref_target_is_a_hard_error() {
	// git rejects a repository whose `HEAD` symlink does not name a ref (`HEAD -> oidfile`). It must never
	// be read as a resolvable symbolic HEAD — that would fabricate a branch/object from an arbitrary file.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("head-nonref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt1 = base.join("wt1");
		let w = work.to_str().unwrap();
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt1.to_str().unwrap(),
		]);
		// Point the linked worktree's HEAD at a non-ref file via symlink.
		let admin_head = work.join(".git/worktrees/wt1/HEAD");
		std::fs::write(work.join(".git/worktrees/wt1/oidfile"), b"whatever\n").unwrap();
		std::fs::remove_file(&admin_head).unwrap();
		symlink("oidfile", &admin_head).unwrap();

		// Enumeration reads that HEAD and must surface a hard error, not a fabricated branch.
		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: a non-ref HEAD symlink must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_healthy_detached_worktree_is_complete_present_not_conflicting() {
	// A valid *detached* worktree (registered, consistent cross-pointers, HEAD at a raw commit) inspected
	// with no expected branch is healthy — it must classify as CompletePresent (branch None, object Some),
	// never as the PartialConflicting "registration missing/inconsistent" state.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("detached-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&["-C", w, "worktree", "add", "--detach", wt.to_str().unwrap()]);
		let head = git(&["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"])
			.trim()
			.to_owned();

		let insp = inspect(&query_no_branch(rid_at(&work), &wt)).await.unwrap();
		assert!(matches!(insp.registration, Registration::Present { .. }));
		match classify(&insp) {
			WorktreeClassification::CompletePresent {
				branch,
				object,
				head: HeadKind::Detached,
			} => {
				assert_eq!(branch, None);
				assert_eq!(object.map(|o| o.to_hex()), Some(head));
			}
			other => panic!("{fmt}: a detached worktree must be CompletePresent, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_healthy_orphan_worktree_is_complete_present_not_conflicting() {
	// A valid *unborn/orphan* worktree (HEAD on a branch whose ref does not yet exist) inspected with no
	// expected branch is healthy — CompletePresent (branch Some, object None), not PartialConflicting.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("orphan-present-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		if !git_ok(&[
			"-C",
			w,
			"worktree",
			"add",
			"--orphan",
			"-b",
			"fresh",
			wt.to_str().unwrap(),
		]) {
			let _ = std::fs::remove_dir_all(&base);
			continue; // git too old for --orphan
		}

		let insp = inspect(&query_no_branch(rid_at(&work), &wt)).await.unwrap();
		assert!(matches!(insp.registration, Registration::Present { .. }));
		match classify(&insp) {
			WorktreeClassification::CompletePresent {
				branch,
				object,
				head: HeadKind::Unborn,
			} => {
				assert_eq!(branch.as_deref(), Some("refs/heads/fresh"));
				assert_eq!(object, None);
			}
			other => panic!("{fmt}: an orphan worktree must be CompletePresent, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_cyclic_symref_chain_is_a_hard_error() {
	// A worktree whose HEAD resolves through a symbolic-ref *cycle* (a → b → a) must surface as malformed,
	// not return an arbitrary mid-chain name that reads as a spurious terminal/unborn ref.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symref-cycle-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// A two-ref symbolic cycle, with the worktree's HEAD pointing into it.
		std::fs::write(work.join(".git/refs/heads/a"), b"ref: refs/heads/b\n").unwrap();
		std::fs::write(work.join(".git/refs/heads/b"), b"ref: refs/heads/a\n").unwrap();
		std::fs::write(work.join(".git/worktrees/wt/HEAD"), b"ref: refs/heads/a\n").unwrap();

		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: a cyclic symref chain must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_unborn_branch_whose_name_is_a_ref_directory_resolves() {
	// git permits an unborn branch `foo` while `refs/heads/foo/bar` exists — then `refs/heads/foo` is a
	// *directory*. `git worktree add --orphan -b foo` creates exactly this. Reading that terminal ref must
	// treat the directory as unborn (non-symbolic), not fail with `IsADirectory`.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("ref-dir-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "foo/bar"]); // makes refs/heads/foo a directory
		let wt = base.join("wt");
		if !git_ok(&[
			"-C",
			w,
			"worktree",
			"add",
			"--orphan",
			"-b",
			"foo",
			wt.to_str().unwrap(),
		]) {
			let _ = std::fs::remove_dir_all(&base);
			continue; // git too old for --orphan
		}
		assert!(
			work.join(".git/refs/heads/foo").is_dir(),
			"{fmt}: refs/heads/foo must be a directory"
		);

		let listing = gitana_linked_worktree::enumerate(&ctx_at(&work))
			.await
			.unwrap();
		let entry = listing
			.entries
			.iter()
			.find(|e| e.branch.as_deref() == Some("refs/heads/foo"))
			.expect("the unborn `foo` worktree");
		assert_eq!(entry.head, Some(HeadKind::Unborn));
		assert!(entry.object.is_none(), "{fmt}: `foo` is unborn");
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_head_symref_target_outside_refs_is_a_hard_error() {
	// git rejects a repository whose `HEAD` names a ref outside `refs/` (`ref: main`, `ref: foo/bar`), even
	// though the name is lexically valid. It must be surfaced as malformed, never a fabricated branch.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("head-outside-refs-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		std::fs::write(work.join(".git/worktrees/wt/HEAD"), b"ref: main\n").unwrap();
		// Oracle: git no longer recognizes the worktree as a repository.
		assert!(
			!git_ok(&["-C", wt.to_str().unwrap(), "status"]),
			"{fmt}: git rejects a non-refs/ HEAD target"
		);

		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: a non-refs/ symref target is a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_head_with_a_non_git_ref_separator_is_a_hard_error() {
	// git accepts only space/tab after `ref:` — a NBSP (or vertical tab) separator makes the repository
	// invalid. Parsing must not silently normalize it (Rust's `trim` would): the odd byte stays in the
	// target, which the refname check then rejects.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("head-sep-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// `ref:` followed by a NBSP (0xC2 0xA0) then the branch.
		std::fs::write(
			work.join(".git/worktrees/wt/HEAD"),
			b"ref:\xc2\xa0refs/heads/feature\n",
		)
		.unwrap();
		// Oracle: git rejects the repository.
		assert!(
			!git_ok(&["-C", wt.to_str().unwrap(), "status"]),
			"{fmt}: git rejects a non-space/tab ref separator"
		);

		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: a non-git ref separator must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_git_invalid_symref_name_is_a_hard_error() {
	// A `HEAD` naming a ref git's `check-ref-format` rejects (e.g. a space in the name) must be surfaced as
	// malformed, never followed to a fabricated healthy unborn branch.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("bad-refname-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Oracle: git rejects this refname.
		assert!(
			!git_ok(&["check-ref-format", "refs/heads/foo bar"]),
			"{fmt}: git rejects a refname with a space"
		);
		std::fs::write(
			work.join(".git/worktrees/wt/HEAD"),
			b"ref: refs/heads/foo bar\n",
		)
		.unwrap();

		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: a git-invalid symref name must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symref_chain_at_gits_depth_limit_matches_git() {
	// git resolves a symbolic-ref chain only within `SYMREF_MAXDEPTH`. A chain one hop too deep is rejected;
	// a chain at the limit resolves. Both must match git's own `symbolic-ref` resolution exactly.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symref-depth-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Chain a1 -> a2 -> a3 -> a4 -> feature (all file symrefs in the shared refs dir).
		for (from, to) in [("a1", "a2"), ("a2", "a3"), ("a3", "a4"), ("a4", "feature")] {
			std::fs::write(
				work.join(format!(".git/refs/heads/{from}")),
				format!("ref: refs/heads/{to}\n"),
			)
			.unwrap();
		}
		let head = work.join(".git/worktrees/wt/HEAD");

		// HEAD -> a1 (chain length exceeds the budget): git rejects, and so must we.
		std::fs::write(&head, b"ref: refs/heads/a1\n").unwrap();
		assert!(
			!git_ok(&["-C", wt.to_str().unwrap(), "symbolic-ref", "HEAD"]),
			"{fmt}: git rejects the over-deep chain"
		);
		assert!(
			gitana_linked_worktree::enumerate(&ctx_at(&work))
				.await
				.is_err(),
			"{fmt}: an over-deep symref chain is a hard error"
		);

		// HEAD -> a2 (chain within the budget): git resolves it to feature, and so must we.
		std::fs::write(&head, b"ref: refs/heads/a2\n").unwrap();
		assert_eq!(
			git(&["-C", wt.to_str().unwrap(), "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feature",
			"{fmt}: git resolves the in-budget chain to feature"
		);
		let listing = gitana_linked_worktree::enumerate(&ctx_at(&work))
			.await
			.unwrap();
		let entry = listing
			.entries
			.iter()
			.find(|e| matches!(e.role, gitana_linked_worktree::WorktreeRole::Linked { .. }))
			.expect("linked worktree");
		assert_eq!(
			entry.branch.as_deref(),
			Some("refs/heads/feature"),
			"{fmt}: the in-budget chain resolves to the terminal branch"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_malformed_gitfile_is_a_hard_error() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("bad-gitfile-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		// Drop the required space in `gitdir: ` — git rejects this as an invalid gitfile format.
		let gitfile = wt.join(".git");
		let admin = std::fs::read_to_string(&gitfile).unwrap();
		std::fs::write(&gitfile, admin.replace("gitdir: ", "gitdir:")).unwrap();

		assert!(
			inspect(&query(rid_at(&work), &wt, "feature"))
				.await
				.is_err(),
			"{fmt}: a malformed .git gitfile must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_gitfile_with_trailing_data_does_not_silently_match() {
	// git takes the *entire* gitfile body after `gitdir: ` (only the trailing line terminator removed) as
	// the path, so a valid first line followed by extra data is a *different* path — one git cannot resolve
	// to the admin. The extra data must not be silently dropped (which would fabricate a healthy, consistent
	// checkout): the pointer no longer matches, so this is an inconsistency/conflict, never CompleteIdempotent.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("gitfile-trailing-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		// Append a second line of garbage after the valid `gitdir: <admin>` line.
		let gitfile = wt.join(".git");
		let admin = std::fs::read_to_string(&gitfile).unwrap();
		std::fs::write(&gitfile, format!("{}EXTRA_GARBAGE\n", admin)).unwrap();
		// Oracle: git no longer sees a valid checkout here (the path with the trailing data does not exist).
		assert!(
			!git_ok(&["-C", wt.to_str().unwrap(), "status"]),
			"{fmt}: git does not resolve a gitfile whose path carries trailing data"
		);

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert!(
			!matches!(
				classify(&insp),
				WorktreeClassification::CompleteIdempotent { .. }
					| WorktreeClassification::MatchingAdvanced { .. }
			),
			"{fmt}: trailing data must not be silently dropped into a healthy match, got {:?}",
			classify(&insp)
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_head_with_no_space_after_ref_resolves() {
	// git accepts a symbolic HEAD whose `ref:` is not followed by a space (`ref:refs/heads/main`); it must
	// resolve as the branch, not be misparsed as an object id and rejected.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("head-nospace-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();
		// Rewrite the worktree's HEAD in the valid no-space form.
		std::fs::write(
			work.join(".git/worktrees/wt/HEAD"),
			b"ref:refs/heads/feature\n",
		)
		.unwrap();
		// Oracle: git still resolves it to `feature`.
		let porcelain = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			porcelain.contains("branch refs/heads/feature"),
			"git resolves the no-space HEAD: {porcelain}"
		);

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		let head = insp.head.expect("a HEAD");
		assert_eq!(head.branch.as_deref(), Some("refs/heads/feature"));
		assert_eq!(head.object.as_ref().map(|o| o.to_hex()), Some(feat));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlinked_lock_marker_does_not_leak_its_target() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("lock-symlink-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		// Point the `locked` marker at a secret file — it must not be followed and exposed as the reason.
		let secret = base.join("secret");
		std::fs::write(&secret, b"TOP SECRET").unwrap();
		let marker = work.join(".git/worktrees/wt/locked");
		symlink(&secret, &marker).unwrap();

		let insp = inspect(&query(rid_at(&work), &wt, "feature"))
			.await
			.unwrap();
		assert_eq!(
			insp.lock,
			gitana_linked_worktree::LockState::Locked { reason: None },
			"{fmt}: a symlinked lock marker must be locked with no reason, never its target's contents"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symref_that_escapes_the_repository_is_rejected() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symref-escape-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt1 = base.join("wt1");
		let w = work.to_str().unwrap();
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt1.to_str().unwrap(),
		]);
		// A HEAD whose symbolic target escapes the repository must not be followed to an ambient file.
		std::fs::write(
			work.join(".git/worktrees/wt1/HEAD"),
			b"ref: ../../../../../../etc/passwd\n",
		)
		.unwrap();

		// Inspecting another destination for `feature` scans worktree HEADs, hitting the escaping one.
		let insp = inspect(&query(rid_at(&work), &base.join("wt2"), "feature")).await;
		assert!(
			insp.is_err(),
			"{fmt}: an escaping symref target must be a hard error"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_relative_common_dir_identity_is_rejected() {
	assert!(
		gitana_linked_worktree::RepositoryId::at_common_dir(std::path::PathBuf::from("rel/.git"))
			.is_err()
	);
}

#[tokio::test]
async fn a_relative_destination_is_rejected() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("relative-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");

		let q = WorktreeQuery {
			repo: rid_at(&work),
			destination: std::path::PathBuf::from("relative/wt"),
			expected_branch: None,
			start: None,
			with_status: false,
			resolve_head: true,
		};
		assert!(
			inspect(&q).await.is_err(),
			"a relative destination must be rejected"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_advanced_branch_matches_without_reset() {
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("advanced-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
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
		let start_hex = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();

		// Advance the branch from inside the worktree.
		let advanced_hex = commit_file(&wt, "b.txt", "2\n", "advance");
		assert_ne!(start_hex, advanced_hex);

		let start = WorktreeObjectId::parse(kind, &start_hex).unwrap();
		let insp = inspect(&query_with_start(rid_at(&work), &wt, "feature", start))
			.await
			.unwrap();
		match classify(&insp) {
			WorktreeClassification::MatchingAdvanced { object, .. } => {
				assert_eq!(
					object.to_hex(),
					advanced_hex,
					"reports current object, no reset"
				);
			}
			other => panic!("{fmt}: expected MatchingAdvanced, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_diverged_branch_is_a_conflict_not_advanced() {
	// When the worktree's branch was rewound/forked onto history that does NOT descend from the requested
	// start, it must NOT be reported as `MatchingAdvanced` (which promises a fast-forward). Inspection
	// computes the ancestry (start is not an ancestor of the current object) and classifies it as a conflict.
	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("diverged-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		let a_hex = commit_file(&work, "a.txt", "1\n", "A");
		let b_hex = commit_file(&work, "b.txt", "2\n", "B"); // main: A -> B
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]); // feature at B
		// Fork feature onto a sibling of B: reset to A, then a *different* commit C (A -> C).
		git(&["-C", wt.to_str().unwrap(), "reset", "--hard", &a_hex]);
		let c_hex = commit_file(&wt, "c.txt", "3\n", "C");
		// Oracle: the requested start B is NOT an ancestor of the current object C.
		assert!(
			!git_ok(&["-C", w, "merge-base", "--is-ancestor", &b_hex, &c_hex]),
			"{fmt}: B must not be an ancestor of C"
		);

		// Request start = B; the worktree is on feature at C, which diverges from B.
		let start = WorktreeObjectId::parse(kind, &b_hex).unwrap();
		let insp = inspect(&query_with_start(rid_at(&work), &wt, "feature", start))
			.await
			.unwrap();
		assert_eq!(
			insp.start_relation,
			Some(gitana_linked_worktree::StartRelation::Diverged)
		);
		match classify(&insp) {
			WorktreeClassification::IdentityConflict {
				detail: IdentityConflict::BranchAtUnexpectedObject { found },
			} => assert_eq!(
				found.to_hex(),
				c_hex,
				"reports the diverged object, never reset"
			),
			other => panic!("{fmt}: a diverged branch must be a conflict, got {other:?}"),
		}
		let _ = std::fs::remove_dir_all(&base);
	}
}
