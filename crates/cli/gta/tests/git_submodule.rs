//! Faithful gitlink / submodule (tree mode `160000`) handling, cross-checked against real git 2.55.
//! A gitlink entry's object id is a *commit* in the submodule's own repository, not a blob here — gitana
//! must record and report it as such (never map it to a `100644` blob).

use std::path::PathBuf;
use std::process::Command;

/// Staging a gitlink (`160000`) and committing must record a real gitlink tree entry — not a `100644`
/// blob pointing at a commit (which `git fsck` rejects). The written tree must be byte-identical to git's.
#[test]
fn commit_preserves_a_gitlink_entry_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Any real commit id serves as the recorded submodule commit — git commits a gitlink without the
	// submodule being present.
	let stage_gitlink = |w: &str| -> String {
		std::fs::write(format!("{w}/a.txt"), b"a\n").unwrap();
		git(w, &["add", "a.txt"]);
		commit(w, "base");
		let sub_commit = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{sub_commit},sub"),
			],
		);
		sub_commit
	};

	let a = unique_tmp("gta-sub-commit-gta");
	let b = unique_tmp("gta-sub-commit-git");
	let (wa, wb) = (a.to_str().unwrap(), b.to_str().unwrap());
	git(
		wa,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(wa, &["config", "user.name", "T"]);
	git(wa, &["config", "user.email", "t@e"]);
	git(
		wb,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(wb, &["config", "user.name", "T"]);
	git(wb, &["config", "user.email", "t@e"]);
	let sub_commit = stage_gitlink(wa);
	stage_gitlink(wb);

	gta(wa, &["commit", "-m", "add submodule"], b"");
	commit(wb, "add submodule");

	// gta's committed tree entry for `sub` must be a gitlink, byte-identical to git's.
	let gta_entry = git(wa, &["ls-tree", "HEAD", "sub"]);
	assert_eq!(
		gta_entry.trim(),
		format!("160000 commit {sub_commit}\tsub"),
		"gta must record a `160000 commit` gitlink, not a blob"
	);
	assert_eq!(
		gta_entry,
		git(wb, &["ls-tree", "HEAD", "sub"]),
		"gta's gitlink tree entry must match git's"
	);
	// git must accept the whole object graph gta wrote.
	assert!(
		Command::new("git")
			.args(["-C", wa, "fsck", "--strict"])
			.output()
			.expect("run git fsck")
			.status
			.success(),
		"git fsck must accept gta's gitlink commit"
	);

	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

/// `status` must treat a real submodule the way git does: a clean one is clean (never a false ` M` nor
/// listed `?? sub/`), and one whose checked-out `HEAD` differs from the recorded commit is ` M sub`.
#[test]
fn status_reports_submodule_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-status");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	// A submodule source with two commits.
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{src}/f"), b"s1\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s1");
	std::fs::write(format!("{src}/f"), b"s2\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s2");
	// A superproject embedding it.
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");

	let norm = |s: String| {
		let mut v: Vec<String> = s.lines().map(str::to_owned).collect();
		v.sort();
		v.join("\n")
	};
	// Clean submodule: both empty.
	assert_eq!(
		norm(gta(&sup, &["status"], b"")),
		norm(git(&sup, &["status", "--porcelain"])),
		"clean submodule status must match git (no false ` M`/`?? sub/`)"
	);
	// Move the submodule's HEAD off the recorded commit → both report ` M sub`.
	git_allow(&format!("{sup}/sub"), &["checkout", "-q", "HEAD~1"]);
	assert_eq!(
		norm(gta(&sup, &["status"], b"")),
		norm(git(&sup, &["status", "--porcelain"])),
		"a moved-HEAD submodule must be ` M sub`, matching git"
	);
	assert!(
		gta(&sup, &["status"], b"").contains("M sub"),
		"the moved submodule must be reported modified"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A submodule pointer change must diff like git: git renders a gitlink as a synthetic
/// `Subproject commit <old>` → `<new>` line, across `diff` (index vs the submodule's checked-out
/// `HEAD`), `diff --cached` (HEAD tree vs index), and `show` (tree vs tree). The submodule working
/// tree is kept clean so git emits no `-dirty` suffix — gitana compares only the recorded commit to
/// the checked-out `HEAD`, matching `status`, which likewise ignores submodule content-dirtiness.
#[test]
fn diff_reports_submodule_pointer_change_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-diff");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	// A submodule source with two commits.
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{src}/f"), b"s1\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s1");
	std::fs::write(format!("{src}/f"), b"s2\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s2");
	// A superproject embedding it at s2.
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");

	// Move the submodule's checked-out HEAD back to s1 with a clean working tree.
	let subdir = format!("{sup}/sub");
	git_allow(&subdir, &["checkout", "-q", "HEAD~1"]);

	// Unstaged: index (s2) vs the submodule's HEAD (s1).
	assert_eq!(
		diff_payload(&gta(&sup, &["diff"], b"")),
		diff_payload(&git(&sup, &["diff"])),
		"unstaged submodule pointer diff must match git"
	);

	// Staged: HEAD tree (s2) vs index (s1).
	git(&sup, &["add", "sub"]);
	assert_eq!(
		diff_payload(&gta(&sup, &["diff", "--cached"], b"")),
		diff_payload(&git(&sup, &["diff", "--cached"])),
		"staged submodule pointer diff must match git"
	);

	// Tree vs tree: commit the pointer change and show it.
	commit(&sup, "move submodule");
	assert_eq!(
		diff_payload(&gta(&sup, &["show", "HEAD"], b"")),
		diff_payload(&git(&sup, &["show", "HEAD"])),
		"committed submodule pointer diff (show) must match git"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Checking out a commit that records a gitlink must materialize it the way git does: create an
/// EMPTY mount directory and record `160000 <commit> 0  sub` in the index, without cloning the
/// submodule (`submodule update` would populate it). Mirrors a clone left without `submodule
/// update` — the gitlink is staged via `cacheinfo`, so there is no `.git/modules` state.
#[test]
fn checkout_materializes_gitlink_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Superproject with a `feat` branch that records a gitlink but no populated submodule; leave the
	// working tree on `main` (no `sub`), so switching back onto `feat` is the materialization under test.
	let setup = |w: &str| -> String {
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "user.name", "T"]);
		git(w, &["config", "user.email", "t@e"]);
		std::fs::write(format!("{w}/root"), b"r\n").unwrap();
		git(w, &["add", "root"]);
		commit(w, "base");
		let c = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(w, &["switch", "-q", "-c", "feat"]);
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{c},sub"),
			],
		);
		commit(w, "addsub");
		git(w, &["switch", "-q", "main"]);
		c
	};
	let a = unique_tmp("gta-sub-co-gta");
	let b = unique_tmp("gta-sub-co-git");
	let (wa, wb) = (a.to_str().unwrap(), b.to_str().unwrap());
	let ca = setup(wa);
	setup(wb);

	// Switch back onto `feat`: gta in one repo, git in the other.
	gta(wa, &["switch", "feat"], b"");
	git(wb, &["switch", "-q", "feat"]);

	// An empty mount directory (no clone).
	assert!(
		a.join("sub").is_dir(),
		"gta must create the submodule mount directory"
	);
	assert!(
		std::fs::read_dir(a.join("sub")).unwrap().next().is_none(),
		"gta must not populate (clone) the submodule"
	);
	// The recorded gitlink, byte-identical to git's.
	assert_eq!(
		git(wa, &["ls-files", "-s", "sub"]).trim(),
		format!("160000 {ca} 0\tsub"),
		"gta must record the gitlink in the index"
	);
	assert_eq!(
		git(wa, &["ls-files", "-s", "sub"]),
		git(wb, &["ls-files", "-s", "sub"]),
		"gta's recorded gitlink must match git's"
	);
	// A clean status afterward, matching git.
	assert_eq!(
		git(wa, &["status", "--porcelain"]),
		git(wb, &["status", "--porcelain"]),
		"status after materializing the gitlink must match git"
	);
	assert!(
		git(wa, &["status", "--porcelain"]).trim().is_empty(),
		"clean after checkout, like git"
	);

	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

/// A submodule mount that is deleted (` D sub`) or replaced by a file (` T sub`) must be reported by
/// `status` and `diff` like git — not hidden as clean. (git splits the type-change diff into a gitlink
/// deletion plus a file addition; gta renders one block, but the added/removed lines are identical, so
/// the `diff_payload` comparison holds.)
#[test]
fn status_and_diff_report_a_removed_or_retyped_gitlink_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> (PathBuf, String) {
		let work = unique_tmp(tag);
		let w = work.to_str().unwrap();
		let src = format!("{w}/src");
		let sup = format!("{w}/super");
		std::fs::create_dir_all(&src).unwrap();
		std::fs::create_dir_all(&sup).unwrap();
		git(
			&src,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(format!("{src}/f"), b"s1\n").unwrap();
		git(&src, &["add", "f"]);
		commit(&src, "s1");
		git(
			&sup,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
		git(&sup, &["add", "root"]);
		commit(&sup, "base");
		git_allow(&sup, &["submodule", "add", "../src", "sub"]);
		commit(&sup, "add submodule");
		(work, sup)
	};

	// Deleted mount → ` D sub`.
	let (work_a, sup_a) = build("gta-sub-del");
	std::fs::remove_dir_all(format!("{sup_a}/sub")).unwrap();
	assert_eq!(
		sorted(&gta(&sup_a, &["status"], b"")),
		sorted(&git(&sup_a, &["status", "--porcelain"])),
		"a deleted gitlink must be ` D sub`, matching git"
	);
	assert_eq!(
		diff_payload(&gta(&sup_a, &["diff"], b"")),
		diff_payload(&git(&sup_a, &["diff"])),
		"a deleted gitlink must diff as a deletion, matching git"
	);
	std::fs::remove_dir_all(&work_a).ok();

	// Mount replaced by a file → ` T sub` (type change).
	let (work_b, sup_b) = build("gta-sub-retype");
	std::fs::remove_dir_all(format!("{sup_b}/sub")).unwrap();
	std::fs::write(format!("{sup_b}/sub"), b"x\n").unwrap();
	assert_eq!(
		sorted(&gta(&sup_b, &["status"], b"")),
		sorted(&git(&sup_b, &["status", "--porcelain"])),
		"a gitlink replaced by a file must be ` T sub`, matching git"
	);
	assert_eq!(
		diff_payload(&gta(&sup_b, &["diff"], b"")),
		diff_payload(&git(&sup_b, &["diff"])),
		"a retyped gitlink's diff lines must match git"
	);
	std::fs::remove_dir_all(&work_b).ok();
}

/// Switching to a branch that does not record the gitlink must succeed the way git does — never
/// `checkout would overwrite local changes to sub` — and leave a populated submodule working tree in
/// place, with status identical to git's afterward.
#[test]
fn switch_away_from_a_gitlink_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> (PathBuf, String) {
		let work = unique_tmp(tag);
		let w = work.to_str().unwrap();
		let src = format!("{w}/src");
		let sup = format!("{w}/super");
		std::fs::create_dir_all(&src).unwrap();
		std::fs::create_dir_all(&sup).unwrap();
		git(
			&src,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(format!("{src}/f"), b"s1\n").unwrap();
		git(&src, &["add", "f"]);
		commit(&src, "s1");
		git(
			&sup,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
		git(&sup, &["add", "root"]);
		commit(&sup, "base");
		git(&sup, &["branch", "nosub"]);
		git_allow(&sup, &["submodule", "add", "../src", "sub"]);
		commit(&sup, "add submodule");
		(work, sup)
	};
	let (work_a, sup_a) = build("gta-sub-swaway-gta");
	let (work_b, sup_b) = build("gta-sub-swaway-git");

	// gta must not refuse (the `gta` helper asserts success); git switches too.
	gta(&sup_a, &["switch", "nosub"], b"");
	git(&sup_b, &["switch", "-q", "nosub"]);

	assert!(
		std::path::Path::new(&format!("{sup_a}/sub/f")).exists(),
		"gta must leave the populated submodule working tree in place"
	);
	assert_eq!(
		sorted(&git(&sup_a, &["status", "--porcelain"])),
		sorted(&git(&sup_b, &["status", "--porcelain"])),
		"status after switching away from the gitlink must match git"
	);
	std::fs::remove_dir_all(&work_a).ok();
	std::fs::remove_dir_all(&work_b).ok();
}

/// A conflicted (unmerged) submodule must report only `UU sub`, never also `?? sub/`: the mount stays
/// excluded from untracked even though the gitlink has no stage-0 index entry during the conflict.
#[test]
fn conflicted_gitlink_status_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-conflict");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	for (body, msg) in [("s1\n", "s1"), ("s2\n", "s2"), ("s3\n", "s3")] {
		std::fs::write(format!("{src}/f"), body).unwrap();
		git(&src, &["add", "f"]);
		commit(&src, msg);
	}
	// Two commits that both differ from src's HEAD (the commit `submodule add` records for the base),
	// so each branch's `cacheinfo` update is a real change git will commit — and they differ from each
	// other, so merging conflicts the gitlink.
	let c1 = git(&src, &["rev-parse", "HEAD~2"]).trim().to_owned();
	let c2 = git(&src, &["rev-parse", "HEAD~1"]).trim().to_owned();
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");
	// Two branches recording different submodule commits, merged to conflict the gitlink.
	git(&sup, &["switch", "-q", "-c", "b2"]);
	git(
		&sup,
		&["update-index", "--cacheinfo", &format!("160000,{c1},sub")],
	);
	commit(&sup, "b2");
	git(&sup, &["switch", "-q", "main"]);
	git(&sup, &["switch", "-q", "-c", "b3"]);
	git(
		&sup,
		&["update-index", "--cacheinfo", &format!("160000,{c2},sub")],
	);
	commit(&sup, "b3");
	// The merge conflicts (exit 1) — run it directly rather than through the success-asserting helper.
	let _ = Command::new("git")
		.args([
			"-C",
			&sup,
			"-c",
			"protocol.file.allow=always",
			"merge",
			"b2",
		])
		.output()
		.expect("run git merge");

	let gta_status = gta(&sup, &["status"], b"");
	assert_eq!(
		sorted(&gta_status),
		sorted(&git(&sup, &["status", "--porcelain"])),
		"a conflicted gitlink must match git (UU sub, no ?? sub/)"
	);
	assert!(
		gta_status.contains("UU sub"),
		"must report the unmerged gitlink"
	);
	assert!(
		!gta_status.contains("?? sub"),
		"must not list the mount as untracked"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// Removing a gitlink whose mount the user replaced with a plain file must LEAVE that file, like git
/// (which only `rmdir`s the mount — "unable to rmdir: Not a directory" — and continues). Deleting the
/// user's file would be data loss git never does.
#[test]
fn switch_away_preserves_a_file_at_the_gitlink_slot() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-slotfile");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	std::fs::write(format!("{w}/root"), b"r\n").unwrap();
	git(w, &["add", "root"]);
	commit(w, "base");
	let c = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(w, &["branch", "nosub"]);
	git(
		w,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c},sub"),
		],
	);
	commit(w, "add gitlink");
	// The user drops a plain file where the mount would be, then switches to the branch without the gitlink.
	std::fs::write(format!("{w}/sub"), b"USERDATA\n").unwrap();
	gta(w, &["switch", "nosub"], b"");
	assert_eq!(
		std::fs::read(format!("{w}/sub")).ok(),
		Some(b"USERDATA\n".to_vec()),
		"the user's file at the removed gitlink slot must be left untouched, like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// Switching to a branch where an ordinary file becomes a gitlink must respect the working tree like
/// git: a CLEAN file is replaced by the empty mount directory, but a DIRTY file blocks the switch — the
/// incoming gitlink is not exempt from cleanliness the way an outgoing/current gitlink is.
#[test]
fn switch_to_a_gitlink_over_a_file_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let setup = |w: &str| {
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "user.name", "T"]);
		git(w, &["config", "user.email", "t@e"]);
		std::fs::write(format!("{w}/root"), b"r\n").unwrap();
		git(w, &["add", "root"]);
		commit(w, "base");
		let c = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(w, &["switch", "-q", "-c", "B"]);
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{c},sub"),
			],
		);
		commit(w, "B");
		git(w, &["switch", "-q", "main"]);
		git(w, &["switch", "-q", "-c", "A"]);
		std::fs::write(format!("{w}/sub"), b"file\n").unwrap();
		git(w, &["add", "sub"]);
		commit(w, "A");
	};

	// Clean file → gitlink: the switch succeeds and `sub` becomes the mount directory.
	let clean = unique_tmp("gta-sub-file2link-clean");
	let wc = clean.to_str().unwrap();
	setup(wc);
	gta(wc, &["switch", "B"], b"");
	assert!(
		clean.join("sub").is_dir(),
		"a clean file must be replaced by the gitlink mount, like git"
	);
	std::fs::remove_dir_all(&clean).ok();

	// Dirty file → gitlink: both gta and git refuse, leaving the file untouched.
	let dirty = unique_tmp("gta-sub-file2link-dirty");
	let wd = dirty.to_str().unwrap();
	setup(wd);
	std::fs::write(format!("{wd}/sub"), b"file\nDIRTY\n").unwrap();
	let gta_out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wd, "switch", "B"])
		.output()
		.expect("run gta");
	assert!(
		!gta_out.status.success(),
		"gta must refuse to overwrite the dirty file with a gitlink, like git"
	);
	let git_out = Command::new("git")
		.args(["-C", wd, "switch", "B"])
		.output()
		.expect("run git");
	assert!(
		!git_out.status.success(),
		"git refuses the same dirty file→gitlink switch"
	);
	assert!(
		dirty.join("sub").is_file(),
		"the dirty file must be left in place after the refusal"
	);
	std::fs::remove_dir_all(&dirty).ok();
}

/// A linked worktree holding only the empty mount directory a gitlink checkout produces is clean, so
/// `gta worktree remove` must remove it WITHOUT `--force` — the empty mount is reconstructable, not a
/// divergence. (Before, the removal-safety classifier could not hash the mount directory and treated it
/// as diverged, refusing the removal.)
#[test]
fn worktree_remove_tolerates_an_empty_gitlink_mount() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-wt");
	let w = work.to_str().unwrap();
	let main = format!("{w}/main");
	std::fs::create_dir_all(&main).unwrap();
	git(
		&main,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{main}/root"), b"r\n").unwrap();
	git(&main, &["add", "root"]);
	commit(&main, "base");
	let c = git(&main, &["rev-parse", "HEAD"]).trim().to_owned();
	git(
		&main,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c},sub"),
		],
	);
	commit(&main, "add gitlink");

	// A linked worktree materializes the empty gitlink mount.
	gta(&main, &["worktree", "add", "../wt2", "-b", "feat"], b"");
	assert!(
		std::path::Path::new(&format!("{w}/wt2/sub")).is_dir(),
		"the gitlink mount is materialized in the linked worktree"
	);
	// The `gta` helper asserts success, so a refusal (which needs `--force`) fails the test.
	gta(&main, &["worktree", "remove", "../wt2"], b"");
	assert!(
		!std::path::Path::new(&format!("{w}/wt2")).exists(),
		"a clean linked worktree with an empty gitlink mount is removed, like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `gta restore <gitlink>` must recreate the empty mount directory the way git does, without reading
/// a blob — the gitlink names a submodule commit, not an object in this repository.
#[test]
fn restore_recreates_a_gitlink_mount_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-restore");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	std::fs::write(format!("{w}/root"), b"r\n").unwrap();
	git(w, &["add", "root"]);
	commit(w, "base");
	let c = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(
		w,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c},sub"),
		],
	);
	commit(w, "add gitlink");

	// The mount is absent in the working tree (cacheinfo staged the gitlink without creating it).
	assert!(!std::path::Path::new(&format!("{w}/sub")).exists());
	// `restore` must not fail on the blob preflight; it creates the empty mount.
	gta(w, &["restore", "sub"], b"");
	assert!(
		std::path::Path::new(&format!("{w}/sub")).is_dir(),
		"restore recreates the empty gitlink mount"
	);
	assert_eq!(
		sorted(&gta(w, &["status"], b"")),
		sorted(&git(w, &["status", "--porcelain"])),
		"status is clean after restoring the gitlink, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// A nested gitlink `a/sub` whose parent slot `a` the user replaced with an untracked file: changing the
/// gitlink's pointer must REFUSE (like git), never unlink `a` to build the mount's parent. The gitlink's
/// own mount is opaque to cleanliness, but ancestor content is still protected — losing `a` is data loss.
#[test]
fn switch_refuses_to_clobber_an_untracked_ancestor_of_a_gitlink() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-ancestor");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	std::fs::write(format!("{w}/root"), b"r\n").unwrap();
	git(w, &["add", "root"]);
	commit(w, "base");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(format!("{w}/root"), b"r2\n").unwrap();
	git(w, &["add", "root"]);
	commit(w, "base2");
	let c2 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	// Branches A and B record the nested gitlink `a/sub` at different commits (a pointer change).
	git(w, &["switch", "-q", "-c", "A"]);
	git(
		w,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c1},a/sub"),
		],
	);
	commit(w, "A");
	git(w, &["switch", "-q", "main"]);
	git(w, &["switch", "-q", "-c", "B"]);
	git(
		w,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c2},a/sub"),
		],
	);
	commit(w, "B");
	// Land on A (materializing the mount), then replace the parent `a/` with an untracked file `a`.
	gta(w, &["switch", "A"], b"");
	std::fs::remove_dir_all(format!("{w}/a")).ok();
	std::fs::write(format!("{w}/a"), b"UNTRACKED\n").unwrap();

	// The pointer-change switch must refuse (assert_cmd directly — the `gta` helper asserts success).
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "B"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"gta must refuse to clobber the untracked ancestor file, like git"
	);
	assert_eq!(
		std::fs::read(format!("{w}/a")).ok(),
		Some(b"UNTRACKED\n".to_vec()),
		"the untracked ancestor file must be preserved"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// The added/removed lines of two diffs, sorted, for order-independent comparison.
fn sorted(text: &str) -> Vec<String> {
	let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
	lines.sort();
	lines
}

/// The semantic content of a unified diff: every added/removed line (sign + text), sorted,
/// ignoring file/hunk headers. Compares gta's diff to git's without depending on git's exact byte
/// framing (notably git's `index <old>..<new>` line, which gta does not emit).
fn diff_payload(text: &str) -> Vec<String> {
	let mut out: Vec<String> = text
		.lines()
		.filter(|l| {
			(l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---")
		})
		.map(str::to_owned)
		.collect();
	out.sort();
	out
}

/// `git` with `protocol.file.allow=always` (modern git blocks `file://` submodule transport by default).
fn git_allow(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir, "-c", "protocol.file.allow=always"];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn commit(dir: &str, msg: &str) {
	// Pin author AND committer dates, not just identity: `commit_preserves_a_gitlink_entry_like_git`
	// compares gitlink commit ids built in two independent repos, so their base commits must be
	// byte-identical — a wall-clock date would make them diverge across a second boundary and flake.
	let out = Command::new("git")
		.args([
			"-C",
			dir,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			msg,
		])
		.env("GIT_AUTHOR_DATE", "1700000000 +0000")
		.env("GIT_COMMITTER_DATE", "1700000000 +0000")
		.output()
		.expect("run git commit");
	assert!(
		out.status.success(),
		"git commit failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}
fn gta(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.write_stdin(stdin.to_vec())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}
fn git(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}
fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}
fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("gta-probe");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
