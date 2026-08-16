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

/// git honours the index trust bits for a gitlink exactly as for a blob: with `--skip-worktree` or
/// `--assume-unchanged` set, a moved submodule HEAD produces NEITHER a ` M sub` status NOR an unstaged
/// pointer diff (probed vs git 2.55; `ls-files -m` likewise stays silent). gta must suppress both too.
#[test]
fn trust_bits_suppress_a_moved_gitlink_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-trust");
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
	// A superproject embedding it at s2, with the mount moved back to s1 (a pointer change).
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");
	git_allow(&format!("{sup}/sub"), &["checkout", "-q", "HEAD~1"]);

	let norm = |s: String| {
		let mut v: Vec<String> = s.lines().map(str::to_owned).collect();
		v.sort();
		v.join("\n")
	};
	// Sanity: without a trust bit the moved pointer IS reported (so the suppression below is meaningful).
	assert_eq!(
		norm(gta(&sup, &["status"], b"")),
		norm(git(&sup, &["status", "--porcelain"])),
		"baseline: moved gitlink pointer must be reported by both"
	);
	assert!(
		gta(&sup, &["status"], b"").contains("M sub"),
		"baseline: the moved submodule must be reported modified"
	);

	// Each trust bit must silence both `status` and `diff` in gta exactly as in git.
	for bit in ["--skip-worktree", "--assume-unchanged"] {
		git(&sup, &["update-index", bit, "sub"]);
		assert_eq!(
			norm(gta(&sup, &["status"], b"")),
			norm(git(&sup, &["status", "--porcelain"])),
			"{bit}: status must match git (both suppressed)"
		);
		assert_eq!(
			diff_payload(&gta(&sup, &["diff"], b"")),
			diff_payload(&git(&sup, &["diff"])),
			"{bit}: unstaged diff must match git (both suppressed)"
		);
		// Clear the bit again so the two bits are tested independently.
		let clear = if bit == "--skip-worktree" {
			"--no-skip-worktree"
		} else {
			"--no-assume-unchanged"
		};
		git(&sup, &["update-index", clear, "sub"]);
	}

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

/// When a tracked NON-gitlink entry `sub` has been locally replaced by a directory and the target
/// branch records `160000 sub`, git REFUSES the switch ("local changes would be overwritten") rather
/// than reusing the directory as the submodule mount — the incoming-mount reuse exemption applies only
/// to a slot with no current tracked owner (probed vs git 2.55). gta must refuse identically, so a
/// switch-back cannot then recursively delete the hidden directory.
#[test]
fn switch_to_a_gitlink_over_a_replaced_blob_dir_refuses_like_git() {
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
		// Branch B: sub is a gitlink.
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
		// Branch A: sub is an ordinary tracked FILE.
		git(w, &["switch", "-q", "-c", "A"]);
		std::fs::write(format!("{w}/sub"), b"file\n").unwrap();
		git(w, &["add", "sub"]);
		commit(w, "A");
		// Locally replace the tracked file with a DIRECTORY holding content.
		std::fs::remove_file(format!("{w}/sub")).unwrap();
		std::fs::create_dir(format!("{w}/sub")).unwrap();
		std::fs::write(format!("{w}/sub/keep.txt"), b"precious\n").unwrap();
	};

	let gdir = unique_tmp("gta-sub-blobdir2link-gta");
	let ddir = unique_tmp("gta-sub-blobdir2link-git");
	let (wg, wd) = (gdir.to_str().unwrap(), ddir.to_str().unwrap());
	setup(wg);
	setup(wd);

	let gta_out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wg, "switch", "B"])
		.output()
		.expect("run gta");
	let git_out = Command::new("git")
		.args(["-C", wd, "switch", "B"])
		.output()
		.expect("run git");
	assert!(
		!git_out.status.success(),
		"git refuses to hide a replaced-blob directory as a submodule mount"
	);
	assert!(
		!gta_out.status.success(),
		"gta must refuse the same switch, not reuse the directory as the mount"
	);
	assert_eq!(
		std::fs::read(format!("{wg}/sub/keep.txt")).ok(),
		Some(b"precious\n".to_vec()),
		"the user's directory content must survive the refusal"
	);
	std::fs::remove_dir_all(&gdir).ok();
	std::fs::remove_dir_all(&ddir).ok();
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

/// `restore` turning a tracked subtree `sub/file` into a gitlink `sub` must stage the change but LEAVE
/// the working `sub/file` — git treats the submodule mount as opaque and never recurses into it to
/// delete descendants (deleting them would be data loss). Status must match git afterward.
#[test]
fn restore_to_a_gitlink_preserves_descendants_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-restore-subtree");
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
	// Branch B records a gitlink at `sub`.
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
	// main has `sub/` as a real subtree.
	git(w, &["switch", "-q", "main"]);
	std::fs::create_dir_all(format!("{w}/sub")).unwrap();
	std::fs::write(format!("{w}/sub/file"), b"content\n").unwrap();
	git(w, &["add", "sub/file"]);
	commit(w, "mainsub");

	gta(
		w,
		&["restore", "--source=B", "--staged", "--worktree", "sub"],
		b"",
	);
	assert_eq!(
		std::fs::read(format!("{w}/sub/file")).ok(),
		Some(b"content\n".to_vec()),
		"the working sub/file must be preserved (the mount is opaque), like git"
	);
	assert_eq!(
		sorted(&gta(w, &["status"], b"")),
		sorted(&git(w, &["status", "--porcelain"])),
		"status after restoring the subtree→gitlink must match git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `gta add` must treat a submodule as opaque like git: `add .` never descends into the mount to stage
/// its contents (which would replace the `160000` gitlink with a `100644` subtree), and `add sub` after
/// the submodule's `HEAD` moves stages the new pointer.
#[test]
fn add_keeps_a_submodule_opaque_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-add");
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
	std::fs::write(format!("{src}/f"), b"s2\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s2");
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");

	// `gta add .` on a clean superproject must leave the gitlink intact and the tree clean.
	gta(&sup, &["add", "."], b"");
	let entry = git(&sup, &["ls-files", "-s", "sub"]);
	assert!(
		entry.starts_with("160000 "),
		"the gitlink must remain a `160000` entry, not a subtree: {entry:?}"
	);
	assert!(
		gta(&sup, &["status"], b"").trim().is_empty(),
		"a clean superproject stays clean after `gta add .`, like git"
	);

	// Move the submodule's HEAD back one; `gta add sub` stages the new pointer, matching git.
	git_allow(&format!("{sup}/sub"), &["checkout", "-q", "HEAD~1"]);
	let head = git(&format!("{sup}/sub"), &["rev-parse", "HEAD"])
		.trim()
		.to_owned();
	gta(&sup, &["add", "sub"], b"");
	assert_eq!(
		git(&sup, &["ls-files", "-s", "sub"]).trim(),
		format!("160000 {head} 0\tsub"),
		"`gta add sub` must stage the submodule's new HEAD pointer, like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `gta merge` when both branches move the same submodule pointer to different commits must record a
/// conflict with three `160000` stages (base/ours/theirs) — never feed the commit ids to blob merging
/// (which fails "object not found", since a submodule's commit lives in the submodule).
#[test]
fn merge_conflicts_on_a_submodule_pointer_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-merge");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	let mut commits = Vec::new();
	for (body, msg) in [("s1\n", "s1"), ("s2\n", "s2"), ("s3\n", "s3")] {
		std::fs::write(format!("{src}/f"), body).unwrap();
		git(&src, &["add", "f"]);
		commit(&src, msg);
		commits.push(git(&src, &["rev-parse", "HEAD"]).trim().to_owned());
	}
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git(
		&sup,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{},sub", commits[0]),
		],
	);
	commit(&sup, "add gitlink");
	git(&sup, &["switch", "-q", "-c", "ours"]);
	git(
		&sup,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{},sub", commits[1]),
		],
	);
	commit(&sup, "ours");
	git(&sup, &["switch", "-q", "main"]);
	git(&sup, &["switch", "-q", "-c", "theirs"]);
	git(
		&sup,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{},sub", commits[2]),
		],
	);
	commit(&sup, "theirs");
	git(&sup, &["switch", "-q", "ours"]);

	// The `gta` helper asserts success; a merge with conflicts exits non-zero, so run it directly.
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", &sup, "merge", "theirs"])
		.output()
		.expect("run gta merge");
	assert!(
		!out.status.success(),
		"a conflicting submodule merge must exit non-zero"
	);
	// The index must carry the three gitlink conflict stages, byte-identical to git's.
	let stages = git(&sup, &["ls-files", "-s", "-u", "sub"]);
	let staged: Vec<(&str, &str)> = stages
		.lines()
		.map(|l| {
			let mut p = l.split_whitespace();
			(p.next().unwrap(), p.nth(1).unwrap()) // (mode, stage)
		})
		.collect();
	assert_eq!(
		staged,
		vec![("160000", "1"), ("160000", "2"), ("160000", "3")],
		"merge must record base/ours/theirs as three 160000 stages, like git: {stages:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// A divergent gitlink-pointer conflict whose mount is ABSENT (an uninitialized submodule) must leave
/// the slot absent like git — `gta merge` records the conflict stages but does NOT materialise a stray
/// empty mount directory, so `gta add .` afterward resolves the absent path as a DELETION (`D sub`),
/// exactly as git does, rather than erroring on an empty mount that "does not have a commit checked out".
#[test]
fn absent_gitlink_conflict_leaves_slot_absent_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Two branches move an uninitialized submodule pointer to different commits; the mount is never
	// checked out. Build with cacheinfo (no real submodule), then merge — gta on one repo, git on another.
	let build = |w: &str| {
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "user.name", "T"]);
		git(w, &["config", "user.email", "t@e"]);
		git(w, &["commit", "-q", "--allow-empty", "-m", "base"]);
		let c0 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(w, &["switch", "-q", "-c", "a"]);
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{c0},sub"),
			],
		);
		commit(w, "a");
		let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(w, &["switch", "-q", "-c", "b", "main"]);
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{c1},sub"),
			],
		);
		commit(w, "b");
		git(w, &["switch", "-q", "a"]);
		// Ensure the mount is genuinely absent.
		std::fs::remove_dir_all(format!("{w}/sub")).ok();
	};

	let g = unique_tmp("gta-sub-absentconf-gta");
	let h = unique_tmp("gta-sub-absentconf-git");
	let (wg, wh) = (g.to_str().unwrap(), h.to_str().unwrap());
	build(wg);
	build(wh);

	// Merge (both expected to conflict, exit non-zero).
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wg, "merge", "b"])
		.output()
		.expect("run gta merge");
	Command::new("git")
		.args(["-C", wh, "-c", "protocol.file.allow=always", "merge", "b"])
		.output()
		.expect("run git merge");

	assert!(
		!g.join("sub").exists(),
		"gta must leave the absent conflicted mount absent, not create an empty directory"
	);
	assert!(
		!h.join("sub").exists(),
		"sanity: git also leaves the absent conflicted mount absent"
	);
	// Resolve with `add .`: both must record a staged deletion of the gitlink.
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wg, "add", "."])
		.output()
		.expect("run gta add");
	Command::new("git")
		.args(["-C", wh, "add", "."])
		.output()
		.expect("run git add");
	assert_eq!(
		git(wg, &["status", "--porcelain", "sub"]).trim(),
		git(wh, &["status", "--porcelain", "sub"]).trim(),
		"add . must resolve the absent conflicted gitlink identically to git (D sub)"
	);
	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&h).ok();
}

/// An INITIALIZED (populated) submodule whose pointer both branches move must conflict and resolve
/// like git: `gta merge` records three `160000` stages even with the mount checked out (it must not
/// abort on the submodule's contents), and `gta add sub` afterward collapses the unmerged stages to a
/// stage-0 gitlink at the submodule's HEAD (never descending into the mount to stage `sub/f`).
#[test]
fn populated_submodule_conflict_and_resolution_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-popmerge");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	// c[0] is the base; c[1] (ours) and c[2] (theirs) DIVERGE from it — neither is an ancestor of the
	// other — so the pointer merge is a genuine conflict. (Deliberate: gitana records a conflict for any
	// divergent submodule-pointer pair and does NOT fast-forward a linear one the way git does — that
	// would require reading the submodule's own commit graph, part of the deferred submodule-operations
	// work, like the `-dirty` submodule-content divergence.)
	let mut c = Vec::new();
	std::fs::write(format!("{src}/f"), b"s1\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s1");
	c.push(git(&src, &["rev-parse", "HEAD"]).trim().to_owned());
	git(&src, &["switch", "-q", "-c", "ours-side"]);
	std::fs::write(format!("{src}/f"), b"s2\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s2");
	c.push(git(&src, &["rev-parse", "HEAD"]).trim().to_owned());
	git(&src, &["switch", "-q", "main"]);
	std::fs::write(format!("{src}/g"), b"s3\n").unwrap();
	git(&src, &["add", "g"]);
	commit(&src, "s3");
	c.push(git(&src, &["rev-parse", "HEAD"]).trim().to_owned());
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	let subdir = format!("{sup}/sub");
	git_allow(&subdir, &["checkout", "-q", &c[0]]);
	git(&sup, &["add", "sub"]);
	commit(&sup, "addsub");
	// ours → s2, theirs → s3 (both differ from base s1 and each other).
	git(&sup, &["switch", "-q", "-c", "ours"]);
	git_allow(&subdir, &["checkout", "-q", &c[1]]);
	git(&sup, &["add", "sub"]);
	commit(&sup, "ours");
	git(&sup, &["switch", "-q", "main"]);
	git(&sup, &["switch", "-q", "-c", "theirs"]);
	git_allow(&subdir, &["checkout", "-q", &c[2]]);
	git(&sup, &["add", "sub"]);
	commit(&sup, "theirs");
	git(&sup, &["switch", "-q", "ours"]);
	git_allow(&subdir, &["checkout", "-q", &c[1]]);

	// Merge must conflict (not abort on the populated mount) and record three 160000 stages.
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", &sup, "merge", "theirs"])
		.output()
		.expect("run gta merge");
	assert!(!out.status.success(), "a submodule conflict exits non-zero");
	let modes_stages: Vec<(String, String)> = git(&sup, &["ls-files", "-s", "-u", "sub"])
		.lines()
		.map(|l| {
			let mut p = l.split_whitespace();
			(p.next().unwrap().to_owned(), p.nth(1).unwrap().to_owned())
		})
		.collect();
	assert_eq!(
		modes_stages,
		vec![
			("160000".to_owned(), "1".to_owned()),
			("160000".to_owned(), "2".to_owned()),
			("160000".to_owned(), "3".to_owned()),
		],
		"populated submodule conflict must record three 160000 stages, like git"
	);

	// Resolve to ours' pointer and `gta add sub`: collapses to a stage-0 gitlink, no `sub/f` blob.
	git_allow(&subdir, &["checkout", "-q", &c[1]]);
	gta(&sup, &["add", "sub"], b"");
	assert_eq!(
		git(&sup, &["ls-files", "-s", "sub"]).trim(),
		format!("160000 {} 0\tsub", c[1]),
		"`gta add sub` must resolve the conflict to a stage-0 gitlink, like git"
	);
	assert!(
		git(&sup, &["ls-files", "sub/f"]).trim().is_empty(),
		"add must not stage the submodule's contents as superproject blobs"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// An explicitly-named `gta add <path-inside-a-submodule>` must fail like git ("Pathspec '…' is in
/// submodule '…'"), not silently succeed — the superproject cannot stage a submodule's own contents.
#[test]
fn add_inside_a_submodule_errors_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-addinside");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{src}/f"), b"s\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s");
	git(
		&sup,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{sup}/root"), b"r\n").unwrap();
	git(&sup, &["add", "root"]);
	commit(&sup, "base");
	git_allow(&sup, &["submodule", "add", "../src", "sub"]);
	commit(&sup, "add submodule");

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", &sup, "add", "sub/f"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"add of a path inside a submodule must fail"
	);
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("Pathspec 'sub/f' is in submodule 'sub'"),
		"error must name the submodule like git: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `add <path-inside>` must be rejected even for a MIXED same-path conflict — a `sub` slot carrying BOTH
/// a blob stage AND a gitlink stage. git decides "is inside a submodule" purely from the index gitlink
/// stage, independent of the same-path blob and of the on-disk `.git` marker (probed vs git 2.55), so an
/// explicit `sub/new` is rejected whether the mount is a real checkout or a marker-free directory. gta
/// must match, never descending to stage `sub/new` as a superproject blob.
#[test]
fn add_inside_a_mixed_gitlink_conflict_errors_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Build a same-path blob-vs-gitlink conflict directly in the index (git RELOCATES such a conflict on a
	// real merge, so we construct the non-relocated stages the way gitana's own merge leaves them).
	let build = |w: &str| {
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
		let blob = String::from_utf8(
			Command::new("git")
				.args(["-C", w, "hash-object", "-w", "--stdin"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped())
				.spawn()
				.and_then(|mut ch| {
					use std::io::Write;
					ch.stdin.take().unwrap().write_all(b"iam a file\n").unwrap();
					ch.wait_with_output()
				})
				.expect("hash-object")
				.stdout,
		)
		.unwrap()
		.trim()
		.to_owned();
		// stage 2 = blob, stage 3 = gitlink, both at `sub`.
		let info = format!("100644 {blob} 2\tsub\n160000 {c} 3\tsub\n");
		let mut ch = Command::new("git")
			.args(["-C", w, "update-index", "--index-info"])
			.stdin(std::process::Stdio::piped())
			.spawn()
			.expect("update-index");
		{
			use std::io::Write;
			ch.stdin.take().unwrap().write_all(info.as_bytes()).unwrap();
		}
		assert!(ch.wait().expect("wait").success());
		// A marker-free directory at the slot, holding an untracked file.
		std::fs::create_dir(format!("{w}/sub")).unwrap();
		std::fs::write(format!("{w}/sub/new"), b"x\n").unwrap();
	};

	let g = unique_tmp("gta-sub-mixedinside-gta");
	let h = unique_tmp("gta-sub-mixedinside-git");
	let (wg, wh) = (g.to_str().unwrap(), h.to_str().unwrap());
	build(wg);
	build(wh);

	let gta_out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wg, "add", "sub/new"])
		.output()
		.expect("run gta");
	let git_out = Command::new("git")
		.args(["-C", wh, "add", "sub/new"])
		.output()
		.expect("run git");
	assert!(
		!git_out.status.success() && !gta_out.status.success(),
		"both git and gta must reject add of a path inside a mixed gitlink conflict"
	);
	assert!(
		String::from_utf8_lossy(&gta_out.stderr).contains("Pathspec 'sub/new' is in submodule 'sub'"),
		"gta must name the submodule like git: {}",
		String::from_utf8_lossy(&gta_out.stderr)
	);
	assert!(
		git(wg, &["ls-files", "sub/new"]).trim().is_empty(),
		"gta must not stage the submodule's contents as a superproject blob"
	);
	std::fs::remove_dir_all(&g).ok();
	std::fs::remove_dir_all(&h).ok();
}

/// Switching AWAY from a branch with an initialized submodule and back must reuse the retained
/// submodule checkout like git — never abort on the populated `sub/…` files. (An arbitrary untracked
/// directory with no `.git` at the slot is still protected; only a real submodule checkout is exempt.)
#[test]
fn switch_back_reuses_a_populated_submodule_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-sub-backreuse");
	let w = work.to_str().unwrap();
	let src = format!("{w}/src");
	let sup = format!("{w}/super");
	std::fs::create_dir_all(&src).unwrap();
	std::fs::create_dir_all(&sup).unwrap();
	git(
		&src,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(format!("{src}/f"), b"s\n").unwrap();
	git(&src, &["add", "f"]);
	commit(&src, "s");
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

	// Away (leaves the populated mount) and back — must succeed and re-record the gitlink, clean.
	gta(&sup, &["switch", "nosub"], b"");
	assert!(
		std::path::Path::new(&format!("{sup}/sub/f")).exists(),
		"the populated submodule is retained on switch-away"
	);
	gta(&sup, &["switch", "main"], b""); // the `gta` helper asserts success — no abort
	assert!(
		std::path::Path::new(&format!("{sup}/sub/f")).exists(),
		"switching back reuses the existing submodule checkout"
	);
	assert!(
		git(&sup, &["ls-files", "-s", "sub"])
			.trim()
			.starts_with("160000 "),
		"the gitlink is re-recorded on the way back"
	);
	assert!(
		gta(&sup, &["status"], b"").trim().is_empty(),
		"clean after the round-trip, like git"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `gta rm` on a submodule (gitlink) must match git: an EMPTY mount directory (the state a plain
/// gitlink checkout leaves) is removed with its index entry WITHOUT --force, an ABSENT mount just drops
/// the index entry, and a POPULATED submodule is refused by a non-force `rm` (git refuses too — via its
/// .gitmodules name lookup, out of scope here). `rm` must never `remove_file` the mount directory.
#[test]
fn rm_removes_a_gitlink_mount_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let cacheinfo = |w: &str| {
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "user.name", "T"]);
		git(w, &["config", "user.email", "t@e"]);
		git(w, &["commit", "-q", "--allow-empty", "-m", "base"]);
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
		commit(w, "addsub");
	};

	// Empty mount: `gta rm sub` removes the index entry and the directory, no --force needed.
	let empty = unique_tmp("gta-sub-rm-empty");
	let we = empty.to_str().unwrap();
	cacheinfo(we);
	std::fs::create_dir_all(format!("{we}/sub")).unwrap();
	gta(we, &["rm", "sub"], b"");
	assert!(
		git(we, &["ls-files", "sub"]).trim().is_empty() && !empty.join("sub").exists(),
		"rm of an empty gitlink mount must drop the index entry and the directory, like git"
	);
	std::fs::remove_dir_all(&empty).ok();

	// Populated real submodule: a non-force `rm` must refuse and keep both the entry and the directory.
	let pop = unique_tmp("gta-sub-rm-pop");
	let wp = pop.to_str().unwrap();
	cacheinfo(wp);
	std::fs::create_dir_all(format!("{wp}/sub")).unwrap();
	git(&format!("{wp}/sub"), &["init", "-q", "-b", "main", "."]);
	git(&format!("{wp}/sub"), &["config", "user.name", "T"]);
	git(&format!("{wp}/sub"), &["config", "user.email", "t@e"]);
	git(
		&format!("{wp}/sub"),
		&["commit", "-q", "--allow-empty", "-m", "x"],
	);
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", wp, "rm", "sub"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"non-force rm of a populated submodule must refuse, like git"
	);
	assert!(
		!git(wp, &["ls-files", "sub"]).trim().is_empty() && pop.join("sub").is_dir(),
		"the refused populated submodule keeps its index entry and working tree"
	);
	std::fs::remove_dir_all(&pop).ok();
}

/// A GLOB or `:(icase)` pathspec rooted inside a tracked submodule must be git's fatal too — `gta add
/// sub/*` / `:(icase)sub/new` → "Pathspec '…' is in submodule 'sub'" — not a silent "did not match".
/// A broad glob that merely crosses the mount (`*` at the root) keeps its silent opacity, like git.
#[test]
fn add_glob_into_a_submodule_errors_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let w = unique_tmp("gta-sub-addglob");
	let ws = w.to_str().unwrap();
	git(
		ws,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(ws, &["config", "user.name", "T"]);
	git(ws, &["config", "user.email", "t@e"]);
	git(ws, &["commit", "-q", "--allow-empty", "-m", "base"]);
	let c = git(ws, &["rev-parse", "HEAD"]).trim().to_owned();
	git(
		ws,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{c},sub"),
		],
	);
	commit(ws, "addsub");
	std::fs::create_dir_all(format!("{ws}/sub")).unwrap();
	std::fs::write(format!("{ws}/sub/new"), b"x\n").unwrap();

	for spec in ["sub/*", ":(icase)sub/new"] {
		let g = assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(["-C", ws, "add", spec])
			.output()
			.expect("run gta");
		let gi = Command::new("git")
			.args(["-C", ws, "add", spec])
			.output()
			.expect("run git");
		assert!(
			!g.status.success() && !gi.status.success(),
			"both must reject `add {spec}` rooted in a submodule"
		);
		assert!(
			String::from_utf8_lossy(&g.stderr).contains("is in submodule 'sub'"),
			"gta must name the submodule for `add {spec}`: {}",
			String::from_utf8_lossy(&g.stderr)
		);
	}
	// A broad glob crossing the mount is NOT an error (silent opacity), matching git.
	let broad = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", ws, "add", "*"])
		.output()
		.expect("run gta");
	assert!(
		broad.status.success(),
		"a broad glob must not error on the submodule boundary, like git"
	);
	std::fs::remove_dir_all(&w).ok();
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
