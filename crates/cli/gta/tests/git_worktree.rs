//! `gta` operating inside a **linked worktree** (`git worktree add`), where `.git` is a file and
//! git splits the repository between a per-worktree directory (`HEAD`, `index`) and a shared common
//! directory (`objects`, `refs`, `config`). gta must read `HEAD` from the worktree but objects and
//! branch refs from the common dir, and a commit must advance only that worktree's branch.
//!
//! Cross-checked against stock `git` so the routing matches git's own behaviour.

use std::path::{Path, PathBuf};
use std::process::Command;

/// gta reads `HEAD`, the branch tip, objects, and config through a linked worktree, and a commit
/// made there advances only that worktree's branch — leaving the main worktree's branch untouched.
#[test]
fn reads_and_commits_through_a_linked_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_linked_worktree("sha256");
}

/// The original report: `gta log` in a SHA-1 linked worktree (stock git's default format) failed
/// with "invalid ref content" before the common/per-worktree split. Lock that in for SHA-1 too.
#[test]
fn reads_and_commits_through_a_sha1_linked_worktree() {
	check_linked_worktree("sha1");
}

fn check_linked_worktree(object_format: &str) {
	let base = unique_tmp(&format!("gta-worktree-{object_format}"));
	let main = base.join("main");
	let wt = base.join("wt");
	let main_s = main.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	// A main repo with a `base` commit on `main`, and a `feature` branch checked out into a linked
	// worktree alongside it.
	std::fs::create_dir_all(&main).unwrap();
	git(
		main_s,
		&[
			"init",
			"-q",
			&format!("--object-format={object_format}"),
			".",
		],
	);
	git(main_s, &["config", "user.name", "T"]);
	git(main_s, &["config", "user.email", "t@e"]);
	std::fs::write(main.join("f.txt"), "base\n").unwrap();
	git(main_s, &["add", "."]);
	git(main_s, &["commit", "-q", "-m", "base"]);
	let base_commit = git(main_s, &["rev-parse", "HEAD"]).trim().to_owned();
	git(main_s, &["branch", "feature"]);
	git(main_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// `.git` in the linked worktree is a file, not a directory — the case gta used to choke on.
	assert!(
		wt.join(".git").is_file(),
		"linked worktree .git should be a file"
	);

	// HEAD is read from the per-worktree dir; the branch tip and objects from the common dir.
	assert_eq!(
		gta(wt_s, &["rev-parse", "HEAD"], b"").trim(),
		git(wt_s, &["rev-parse", "HEAD"]).trim(),
	);
	assert_eq!(gta(wt_s, &["rev-parse", "HEAD"], b"").trim(), base_commit);
	// A read that walks the graph (objects live in the common dir) names the base commit.
	assert!(gta(wt_s, &["log"], b"").contains(&base_commit));
	// A clean worktree matches git's porcelain (empty).
	assert_eq!(
		gta(wt_s, &["status"], b""),
		git(wt_s, &["status", "--porcelain"])
	);

	// A commit made in the linked worktree: writes a blob to the common object store and advances the
	// worktree's branch (`feature`), not the main worktree's (`main`).
	std::fs::write(wt.join("g.txt"), "from-worktree\n").unwrap();
	gta(wt_s, &["add", "g.txt"], b"");
	let new_commit = gta(wt_s, &["commit", "-m", "add g"], b"").trim().to_owned();

	// `feature` moved to the new commit; `main` did not move.
	assert_eq!(git(main_s, &["rev-parse", "feature"]).trim(), new_commit);
	assert_eq!(git(main_s, &["rev-parse", "main"]).trim(), base_commit);
	// The new commit and its blob are stored where stock git can read them (the common object store).
	assert_eq!(
		git(wt_s, &["cat-file", "-p", "HEAD:g.txt"]),
		"from-worktree\n"
	);
	assert_eq!(git(wt_s, &["rev-parse", "HEAD"]).trim(), new_commit);

	std::fs::remove_dir_all(&base).ok();
}

/// A branch's ref is shared across worktrees, so gta must refuse to check out a branch already
/// checked out in another worktree — as git does — rather than putting two worktrees on one branch.
#[test]
fn switch_refuses_a_branch_checked_out_in_another_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let base = unique_tmp("gta-worktree-switch");
	let main = base.join("main");
	let wt = base.join("wt");
	let main_s = main.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	std::fs::create_dir_all(&main).unwrap();
	git(main_s, &["init", "-q", "--object-format=sha256", "."]);
	git(main_s, &["config", "user.name", "T"]);
	git(main_s, &["config", "user.email", "t@e"]);
	std::fs::write(main.join("f.txt"), "base\n").unwrap();
	git(main_s, &["add", "."]);
	git(main_s, &["commit", "-q", "-m", "base"]);
	git(main_s, &["branch", "feature"]);
	git(main_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// The linked worktree cannot switch to `main` (held by the main worktree), and the main worktree
	// cannot switch to `feature` (held by the linked worktree) — both refused, HEADs unmoved.
	assert!(gta_fail(wt_s, &["switch", "main"]).contains("already checked out"));
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"feature"
	);

	assert!(gta_fail(main_s, &["switch", "feature"]).contains("already checked out"));
	assert_eq!(
		git(main_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"main"
	);

	// A branch no other worktree holds is fine: creating and switching to a fresh branch works.
	gta(wt_s, &["switch", "-c", "fresh"], b"");
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"fresh"
	);

	std::fs::remove_dir_all(&base).ok();
}

/// A *bare* common repository has no main working tree, so its symbolic `HEAD` is not a checkout —
/// a linked worktree may switch to that branch. The bare flag must be read with git's boolean
/// grammar (`core.bare = yes`, not just `true`), or gta would wrongly treat the bare HEAD as a
/// second checkout and refuse.
#[test]
fn switch_allows_the_branch_named_by_a_bare_repo_head() {
	let base = unique_tmp("gta-worktree-bare");
	let seed = base.join("seed");
	let bare = base.join("bare.git");
	let wt = base.join("wt");
	let base_s = base.to_str().unwrap();
	let seed_s = seed.to_str().unwrap();
	let bare_s = bare.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	// A bare repo (default sha1) whose HEAD names `main`, with a `feature` branch parked in a linked
	// worktree. `core.bare` is written in git's `yes` form, not the literal `true`.
	std::fs::create_dir_all(&seed).unwrap();
	git(seed_s, &["init", "-q", "-b", "main", "."]);
	git(seed_s, &["config", "user.name", "T"]);
	git(seed_s, &["config", "user.email", "t@e"]);
	std::fs::write(seed.join("f.txt"), "base\n").unwrap();
	git(seed_s, &["add", "."]);
	git(seed_s, &["commit", "-q", "-m", "base"]);
	git(base_s, &["clone", "-q", "--bare", seed_s, "bare.git"]);
	git(bare_s, &["config", "core.bare", "yes"]);
	git(bare_s, &["branch", "feature", "main"]);
	git(bare_s, &["worktree", "add", "-q", wt_s, "feature"]);

	// git allows it (the bare HEAD is not a checkout); gta must too.
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"feature"
	);
	gta(wt_s, &["switch", "main"], b"");
	assert_eq!(
		git(wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"main"
	);

	std::fs::remove_dir_all(&base).ok();
}

/// In-progress operation state (here, a rebase) is per-worktree: a rebase started in a linked
/// worktree must be invisible to — and not abortable from — another worktree, and must move only its
/// own branch. (Regression: the state files were once routed to the shared common dir, so an abort in
/// the main worktree moved the linked worktree's branch.)
#[test]
fn rebase_state_is_isolated_per_worktree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let base = unique_tmp("gta-worktree-rebase");
	let main = base.join("main");
	let wt = base.join("wt");
	let main_s = main.to_str().unwrap();
	let wt_s = wt.to_str().unwrap();

	// `main` and `feature` diverge on f.txt (so a rebase conflicts); `feature` lives in a linked wt.
	std::fs::create_dir_all(&main).unwrap();
	git(main_s, &["init", "-q", "--object-format=sha256", "."]);
	git(main_s, &["config", "user.name", "T"]);
	git(main_s, &["config", "user.email", "t@e"]);
	std::fs::write(main.join("f.txt"), "base\n").unwrap();
	git(main_s, &["add", "."]);
	git(main_s, &["commit", "-q", "-m", "A"]);
	git(main_s, &["branch", "feature"]);
	git(main_s, &["worktree", "add", "-q", wt_s, "feature"]);
	std::fs::write(main.join("f.txt"), "main\n").unwrap();
	git(main_s, &["commit", "-q", "-am", "M"]);
	let main_tip = git(main_s, &["rev-parse", "main"]).trim().to_owned();
	std::fs::write(wt.join("f.txt"), "feature\n").unwrap();
	git(wt_s, &["commit", "-q", "-am", "F"]);
	let feature_orig = git(wt_s, &["rev-parse", "feature"]).trim().to_owned();

	// Start a rebase in the linked worktree; replaying F onto M conflicts and stops.
	gta_fail(wt_s, &["rebase", "main"]);
	assert!(
		!main.join(".git/REBASE_TODO").exists(),
		"rebase state must not leak into the shared common dir"
	);

	// The main worktree has no rebase of its own: an --abort there must refuse and leave `main` put.
	gta_fail(main_s, &["rebase", "--abort"]);
	assert_eq!(git(main_s, &["rev-parse", "main"]).trim(), main_tip);

	// The linked worktree aborts its own rebase, restoring `feature` to F.
	gta(wt_s, &["rebase", "--abort"], b"");
	assert_eq!(git(main_s, &["rev-parse", "feature"]).trim(), feature_orig);
	assert_eq!(git(main_s, &["rev-parse", "main"]).trim(), main_tip);

	std::fs::remove_dir_all(&base).ok();
}

/// The full create/inspect/destroy lifecycle of `gta worktree`, cross-checked against stock git: a
/// gta-created worktree is a layout git reads and operates in, and `list --porcelain` is byte-for-byte
/// git's.
#[test]
fn adds_lists_and_removes_worktrees_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_add_list_remove("sha256");
}

#[test]
fn adds_lists_and_removes_worktrees_sha1() {
	check_add_list_remove("sha1");
}

fn check_add_list_remove(object_format: &str) {
	let base = unique_tmp(&format!("gta-wt-life-{object_format}"));
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	// A gta-initialised repository with one commit on `main`.
	gta(
		base_s,
		&["init", &format!("--object-format={object_format}"), repo_s],
		b"",
	);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");
	let base_commit = gta(repo_s, &["rev-parse", "HEAD"], b"").trim().to_owned();

	// DWIM add: a new branch named after the destination's basename, checked out at HEAD.
	let wt_a = base.join("wt-a");
	let wt_a_s = wt_a.to_str().unwrap();
	gta(repo_s, &["worktree", "add", wt_a_s], b"");
	assert!(
		wt_a.join(".git").is_file(),
		"linked worktree .git is a file"
	);
	assert!(wt_a.join("f.txt").is_file(), "the tree is checked out");
	assert_eq!(gta(wt_a_s, &["rev-parse", "HEAD"], b"").trim(), base_commit);
	assert_eq!(git(repo_s, &["rev-parse", "wt-a"]).trim(), base_commit);

	// A detached add, and an explicit `-b` new-branch add.
	let wt_det = base.join("wt-det");
	gta(
		repo_s,
		&[
			"worktree",
			"add",
			"--detach",
			wt_det.to_str().unwrap(),
			"HEAD",
		],
		b"",
	);
	let wt_feat = base.join("wt-feat");
	gta(
		repo_s,
		&[
			"worktree",
			"add",
			"-b",
			"feature",
			wt_feat.to_str().unwrap(),
		],
		b"",
	);
	assert_eq!(git(repo_s, &["rev-parse", "feature"]).trim(), base_commit);

	// `gta worktree list --porcelain` is exactly stock git's, and stock git reads and operates in the
	// gta-created worktree (its layout is fully git-compatible).
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	assert_eq!(git(wt_a_s, &["rev-parse", "HEAD"]).trim(), base_commit);
	assert_eq!(git(wt_a_s, &["status", "--porcelain"]), "");

	// Remove a clean worktree: gone from disk and from git's listing.
	gta(
		repo_s,
		&["worktree", "remove", wt_feat.to_str().unwrap()],
		b"",
	);
	assert!(!wt_feat.exists());
	assert!(!git(repo_s, &["worktree", "list", "--porcelain"]).contains("wt-feat"));

	// A dirty worktree is refused without `--force`, removed with it.
	std::fs::write(wt_a.join("f.txt"), "dirty\n").unwrap();
	assert!(gta_fail(repo_s, &["worktree", "remove", wt_a_s]).contains("modified or untracked"),);
	gta(repo_s, &["worktree", "remove", "--force", wt_a_s], b"");
	assert!(!wt_a.exists());

	// The main worktree cannot be removed.
	assert!(gta_fail(repo_s, &["worktree", "remove", "."]).contains("main working tree"));

	std::fs::remove_dir_all(&base).ok();
}

/// A branch's ref is shared across worktrees, so `worktree add` must refuse a branch already checked
/// out elsewhere — as git does — and, unlike git, must not leave a dangling admin directory behind.
#[test]
fn add_refuses_a_branch_checked_out_elsewhere() {
	let base = unique_tmp("gta-wt-guard");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	gta(
		repo_s,
		&["worktree", "add", base.join("wt-a").to_str().unwrap()],
		b"",
	);
	let before = gta(repo_s, &["worktree", "list", "--porcelain"], b"");

	// The `wt-a` branch is held by its worktree, and `main` by the main worktree: both refused.
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"add",
				base.join("dup").to_str().unwrap(),
				"wt-a"
			]
		)
		.contains("already checked out"),
	);
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"add",
				base.join("dup2").to_str().unwrap(),
				"main"
			]
		)
		.contains("already checked out"),
	);

	// No worktree was registered and no destination directory was left behind.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		before
	);
	assert!(!base.join("dup").exists() && !base.join("dup2").exists());

	std::fs::remove_dir_all(&base).ok();
}

/// An annotated tag is a valid start point: `add` peels it to the commit it names (git accepts
/// annotated tags as commit-ish), detaching HEAD at that commit rather than at the tag object.
#[test]
fn add_peels_an_annotated_tag_start_point() {
	let base = unique_tmp("gta-wt-tag");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");
	git(repo_s, &["tag", "-a", "v1", "-m", "release"]);

	let wt = base.join("wt-tag");
	gta(
		repo_s,
		&["worktree", "add", wt.to_str().unwrap(), "v1"],
		b"",
	);

	// HEAD is the peeled commit (not the tag object), and stock git reads it.
	let peeled = git(repo_s, &["rev-parse", "v1^{commit}"]).trim().to_owned();
	assert_eq!(
		git(wt.to_str().unwrap(), &["rev-parse", "HEAD"]).trim(),
		peeled
	);
	assert_ne!(
		git(repo_s, &["rev-parse", "v1"]).trim(),
		peeled,
		"the tag object is not a commit"
	);

	std::fs::remove_dir_all(&base).ok();
}

/// In a repository with no commits (unborn HEAD), `add` infers an orphan worktree as git does: a new
/// unborn branch, an empty checkout, and no branch ref yet. `list` shows the unborn entries with an
/// all-zeros HEAD, and a first commit there is born on the branch and readable by stock git.
#[test]
fn add_creates_an_orphan_worktree_from_an_unborn_head() {
	let base = unique_tmp("gta-wt-orphan");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);

	// The main worktree is unborn; `list --porcelain` still lists it with an all-zeros HEAD, matching
	// git byte-for-byte.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// An orphan add: a new unborn branch named after the basename, an empty checkout, no ref yet.
	let wt = base.join("owt");
	let wt_s = wt.to_str().unwrap();
	gta(repo_s, &["worktree", "add", wt_s], b"");
	assert!(wt.join(".git").is_file());
	assert_eq!(
		std::fs::read_to_string(repo.join(".git/worktrees/owt/HEAD"))
			.unwrap()
			.trim(),
		"ref: refs/heads/owt"
	);
	assert!(
		!repo.join(".git/refs/heads/owt").exists(),
		"the branch is unborn (no ref yet)"
	);
	// Both unborn worktrees now listed, still matching git exactly.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	// stock git operates in the orphan worktree (its unborn HEAD names the orphan branch).
	assert_eq!(
		git(wt_s, &["symbolic-ref", "--short", "HEAD"]).trim(),
		"owt"
	);

	// An unborn branch has no ref to race on, so git allows a second worktree pointing at the main
	// worktree's own (still unborn) branch — the checked-out-elsewhere guard must not fire here.
	let main_branch = git(repo_s, &["symbolic-ref", "--short", "HEAD"])
		.trim()
		.to_owned();
	let dup = base.join("dup");
	gta(
		repo_s,
		&["worktree", "add", "-b", &main_branch, dup.to_str().unwrap()],
		b"",
	);
	assert_eq!(
		std::fs::read_to_string(repo.join(".git/worktrees/dup/HEAD"))
			.unwrap()
			.trim(),
		format!("ref: refs/heads/{main_branch}"),
	);

	// A first commit in the orphan is born on its branch and readable by stock git.
	std::fs::write(wt.join("g.txt"), "hi\n").unwrap();
	gta(wt_s, &["add", "g.txt"], b"");
	let commit = gta(wt_s, &["commit", "-m", "first"], b"").trim().to_owned();
	assert_eq!(git(repo_s, &["rev-parse", "owt"]).trim(), commit);

	std::fs::remove_dir_all(&base).ok();
}

/// A bare repository has no main working tree: `list` leads with the bare repo's own `bare` entry,
/// then its linked worktrees — byte-for-byte git's output.
#[test]
fn list_reports_a_bare_repository_and_its_worktrees() {
	let base = unique_tmp("gta-wt-bare");
	let base_s = base.to_str().unwrap();
	let seed = base.join("seed");
	let seed_s = seed.to_str().unwrap();
	let bare = base.join("bare.git");
	let bare_s = bare.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", seed_s], b"");
	git(seed_s, &["config", "user.name", "T"]);
	git(seed_s, &["config", "user.email", "t@e"]);
	std::fs::write(seed.join("f.txt"), "base\n").unwrap();
	gta(seed_s, &["add", "."], b"");
	gta(seed_s, &["commit", "-m", "base"], b"");

	git(base_s, &["clone", "--bare", seed_s, bare_s]);
	git(
		bare_s,
		&["worktree", "add", base.join("bwt").to_str().unwrap()],
	);

	assert_eq!(
		gta(bare_s, &["worktree", "list", "--porcelain"], b""),
		git(bare_s, &["worktree", "list", "--porcelain"]),
	);
	assert_eq!(
		gta(bare_s, &["worktree", "list"], b""),
		git(bare_s, &["worktree", "list"])
	);

	std::fs::remove_dir_all(&base).ok();
}

/// A branch name git rejects (via `check-ref-format`) is refused before any ref or admin layout is
/// written — both a path-derived DWIM name and an explicit `-b` name — so gta never writes a broken
/// `refs/heads/…` that stock git would then choke on.
#[test]
fn add_rejects_invalid_branch_names() {
	let base = unique_tmp("gta-wt-badname");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	// A DWIM branch from a basename with a space, and an explicit `-b` name with a forbidden char.
	let spaced = base.join("wt space");
	assert!(
		gta_fail(repo_s, &["worktree", "add", spaced.to_str().unwrap()])
			.contains("not a valid branch name"),
	);
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"add",
				"-b",
				"bad~name",
				base.join("wtb").to_str().unwrap()
			]
		)
		.contains("not a valid branch name"),
	);
	// `HEAD` is reserved: git rejects it as a branch name too.
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"add",
				"-b",
				"HEAD",
				base.join("wth").to_str().unwrap()
			]
		)
		.contains("not a valid branch name"),
	);

	// Nothing was written: no worktree registered, no destination directory created.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	assert!(!spaced.exists() && !base.join("wtb").exists());
	// And git sees no broken ref (show-ref runs clean and names neither rejected branch).
	let refs = git(repo_s, &["show-ref"]);
	assert!(!refs.contains("wt space") && !refs.contains("bad~name"));

	std::fs::remove_dir_all(&base).ok();
}

/// git removes a *stale* worktree whose checkout has been deleted, cleaning up the admin entry (and
/// freeing its branch); gta must too, rather than failing because the checkout path no longer exists.
#[test]
fn remove_cleans_up_a_stale_worktree() {
	let base = unique_tmp("gta-wt-stale");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let wt = base.join("gone");
	gta(repo_s, &["worktree", "add", wt.to_str().unwrap()], b"");
	// Delete the checkout out from under the registration, leaving a stale admin entry.
	std::fs::remove_dir_all(&wt).unwrap();
	assert!(repo.join(".git/worktrees/gone").exists());

	gta(repo_s, &["worktree", "remove", wt.to_str().unwrap()], b"");

	// The admin entry is gone and git no longer lists the worktree.
	assert!(!repo.join(".git/worktrees/gone").exists());
	assert!(!git(repo_s, &["worktree", "list", "--porcelain"]).contains("gone"));

	std::fs::remove_dir_all(&base).ok();
}

/// When a registered checkout directory still exists but its `.git` file is gone (or foreign),
/// removal is refused — even with `--force` — so an unrelated directory left at the recorded path is
/// never destroyed, matching git's validation guard.
#[test]
fn remove_refuses_a_checkout_with_a_missing_gitfile() {
	let base = unique_tmp("gta-wt-corrupt");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let wt = base.join("corrupt");
	gta(repo_s, &["worktree", "add", wt.to_str().unwrap()], b"");
	// Delete the checkout's gitfile but leave the directory with an unrelated file in it.
	std::fs::remove_file(wt.join(".git")).unwrap();
	std::fs::write(wt.join("keep.txt"), "precious\n").unwrap();

	// Refused with and without --force; the directory and its file survive.
	assert!(
		gta_fail(repo_s, &["worktree", "remove", wt.to_str().unwrap()]).contains("validation failed")
	);
	assert!(
		gta_fail(
			repo_s,
			&["worktree", "remove", "--force", wt.to_str().unwrap()]
		)
		.contains("validation failed"),
	);
	assert!(wt.join("keep.txt").exists(), "unrelated file preserved");

	std::fs::remove_dir_all(&base).ok();
}

/// A worktree stock git created with a *relative* `gitdir` pointer (`worktree.useRelativePaths`) is
/// resolved against the admin directory, not the process cwd, so `list` reports its real path (not a
/// spurious `prunable`) and `remove` finds it — byte-for-byte git's listing either way.
#[test]
fn handles_relative_gitdir_worktrees() {
	let base = unique_tmp("gta-wt-rel");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	std::fs::create_dir_all(&repo).unwrap();
	git(repo_s, &["init", "-q", "--object-format=sha1", "."]);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	git(repo_s, &["add", "."]);
	git(repo_s, &["commit", "-q", "-m", "base"]);

	// git writes a relative `gitdir` under this config (on a git new enough to support it).
	git(repo_s, &["config", "worktree.useRelativePaths", "true"]);
	let rel = base.join("relwt");
	git(repo_s, &["worktree", "add", rel.to_str().unwrap(), "HEAD"]);

	// gta resolves the relative pointer the same way git does: identical listing, no false prunable.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	// And `remove` matches the checkout by its resolved path.
	gta(repo_s, &["worktree", "remove", rel.to_str().unwrap()], b"");
	assert!(!rel.exists());
	assert!(!git(repo_s, &["worktree", "list", "--porcelain"]).contains("relwt"));

	std::fs::remove_dir_all(&base).ok();
}

/// `list` reports git's `locked [<reason>]` and `prunable <reason>` attributes, and `remove` honors a
/// lock the way git does: refused (even with one `-f`) until a second `-f`. The lock is created by
/// stock `git worktree lock`, so this also proves interop with git-authored lock state.
#[test]
fn honors_locked_and_prunable_worktree_state() {
	let base = unique_tmp("gta-wt-lock");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	// A locked worktree (with a reason), a locked worktree (no reason), and a stale/prunable one.
	let locked = base.join("locked");
	gta(repo_s, &["worktree", "add", locked.to_str().unwrap()], b"");
	git(
		repo_s,
		&[
			"worktree",
			"lock",
			"--reason",
			"in use",
			locked.to_str().unwrap(),
		],
	);
	let plain = base.join("plain");
	gta(repo_s, &["worktree", "add", plain.to_str().unwrap()], b"");
	git(repo_s, &["worktree", "lock", plain.to_str().unwrap()]);
	let stale = base.join("stale");
	gta(
		repo_s,
		&[
			"worktree",
			"add",
			"--detach",
			stale.to_str().unwrap(),
			"HEAD",
		],
		b"",
	);
	std::fs::remove_dir_all(&stale).unwrap();
	// A locked *and* stale worktree: git reports only `locked` (the lock protects it from pruning), not
	// `prunable`.
	let held = base.join("held");
	gta(
		repo_s,
		&[
			"worktree",
			"add",
			"--detach",
			held.to_str().unwrap(),
			"HEAD",
		],
		b"",
	);
	git(repo_s, &["worktree", "lock", held.to_str().unwrap()]);
	std::fs::remove_dir_all(&held).unwrap();

	// Both list forms match git byte-for-byte, including the locked reason and prunable marker.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	assert_eq!(
		gta(repo_s, &["worktree", "list"], b""),
		git(repo_s, &["worktree", "list"])
	);

	// A locked worktree resists removal until a second `-f`, and the message carries the lock reason.
	assert!(
		gta_fail(repo_s, &["worktree", "remove", locked.to_str().unwrap()])
			.contains("locked working tree, lock reason: in use"),
	);
	assert!(
		gta_fail(
			repo_s,
			&["worktree", "remove", "-f", locked.to_str().unwrap()]
		)
		.contains("locked working tree"),
	);
	assert!(locked.exists(), "still present after one -f");
	gta(
		repo_s,
		&["worktree", "remove", "-ff", locked.to_str().unwrap()],
		b"",
	);
	assert!(!locked.exists(), "removed with -ff");

	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn locks_and_unlocks_worktrees_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_lock_unlock("sha256");
}

#[test]
fn locks_and_unlocks_worktrees_sha1() {
	check_lock_unlock("sha1");
}

/// `lock`/`unlock` match git: they write/remove `<admin>/locked` with git's exact reason format,
/// interoperate with git-authored lock state (each tool reads the other's lock), resolve a worktree
/// by a bare name as well as a path, and reject the main worktree, an unknown worktree, and a
/// double-lock/double-unlock the way git does.
fn check_lock_unlock(object_format: &str) {
	let base = unique_tmp(&format!("gta-wt-lock2-{object_format}"));
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(
		base_s,
		&["init", &format!("--object-format={object_format}"), repo_s],
		b"",
	);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let a = base.join("a");
	let b = base.join("b");
	gta(repo_s, &["worktree", "add", a.to_str().unwrap()], b"");
	gta(repo_s, &["worktree", "add", b.to_str().unwrap()], b"");

	// Lock with a reason writes git's exact `<reason>\n` body; both list forms then match git.
	gta(
		repo_s,
		&[
			"worktree",
			"lock",
			"--reason",
			"busy building",
			a.to_str().unwrap(),
		],
		b"",
	);
	assert_eq!(
		std::fs::read_to_string(repo.join(".git/worktrees/a/locked")).unwrap(),
		"busy building\n",
	);
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	// Re-locking is refused, echoing the recorded reason.
	assert!(
		gta_fail(repo_s, &["worktree", "lock", a.to_str().unwrap()])
			.contains("is already locked, reason: busy building"),
	);

	// A bare name resolves like a path (git's `find_worktree` suffix match).
	gta(repo_s, &["worktree", "lock", "b"], b"");
	assert!(repo.join(".git/worktrees/b/locked").exists());

	// git reads gta's lock: it refuses to re-lock, then unlocks it.
	assert!(
		!Command::new("git")
			.args(["-C", repo_s, "worktree", "lock", b.to_str().unwrap()])
			.status()
			.unwrap()
			.success(),
		"git should refuse to re-lock a gta-locked worktree",
	);
	git(repo_s, &["worktree", "unlock", b.to_str().unwrap()]);
	assert!(!repo.join(".git/worktrees/b/locked").exists());

	// gta reads git's lock: git locks (no reason), gta unlocks by name.
	git(repo_s, &["worktree", "lock", b.to_str().unwrap()]);
	gta(repo_s, &["worktree", "unlock", "b"], b"");
	assert!(!repo.join(".git/worktrees/b/locked").exists());

	// Unlock the reasoned lock, then a second unlock is refused.
	gta(repo_s, &["worktree", "unlock", a.to_str().unwrap()], b"");
	assert!(!repo.join(".git/worktrees/a/locked").exists());
	assert!(gta_fail(repo_s, &["worktree", "unlock", a.to_str().unwrap()]).contains("is not locked"),);

	// The main worktree and an unknown worktree are rejected as git rejects them.
	assert!(
		gta_fail(repo_s, &["worktree", "lock", "."]).contains("main working tree cannot be locked"),
	);
	assert!(
		gta_fail(repo_s, &["worktree", "unlock", "."]).contains("main working tree cannot be locked"),
	);
	assert!(gta_fail(repo_s, &["worktree", "lock", "ghost"]).contains("is not a working tree"));

	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn prunes_worktrees_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_prune("sha256");
}

#[test]
fn prunes_worktrees_sha1() {
	check_prune("sha1");
}

/// `prune` matches git: it removes the admin dirs of worktrees whose checkout is gone, keeps locked
/// and fresh ones, honours `--dry-run`, and honours `--expire` by comparing the per-worktree `index`
/// mtime (a bare integer is epoch seconds to both tools). Reports match git byte-for-byte.
fn check_prune(object_format: &str) {
	let base = unique_tmp(&format!("gta-wt-prune-{object_format}"));
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(
		base_s,
		&["init", &format!("--object-format={object_format}"), repo_s],
		b"",
	);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	// A fresh worktree, a stale one (checkout removed), and a locked-then-stale one.
	let keep = base.join("keep");
	let stale = base.join("stale");
	let held = base.join("held");
	for path in [&keep, &stale, &held] {
		gta(repo_s, &["worktree", "add", path.to_str().unwrap()], b"");
	}
	git(repo_s, &["worktree", "lock", held.to_str().unwrap()]);
	std::fs::remove_dir_all(&stale).unwrap();
	std::fs::remove_dir_all(&held).unwrap();

	// A dry run reports exactly what git reports (sorted, so it is independent of readdir order) and
	// removes nothing.
	assert_eq!(
		sorted_lines(&gta_stderr(repo_s, &["worktree", "prune", "-n", "-v"])),
		sorted_lines(&git_stderr(repo_s, &["worktree", "prune", "-n", "-v"])),
	);
	assert!(
		repo.join(".git/worktrees/stale").exists(),
		"dry run kept the admin dir",
	);

	// A real prune removes the stale admin dir, keeps the locked-stale and the fresh ones.
	gta_stderr(repo_s, &["worktree", "prune", "-v"]);
	assert!(!repo.join(".git/worktrees/stale").exists());
	assert!(
		repo.join(".git/worktrees/held").exists(),
		"a locked worktree is protected from pruning",
	);
	assert!(repo.join(".git/worktrees/keep").exists());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// `--expire` keeps a stale worktree whose index is newer than the cutoff and prunes an older one;
	// gta and git agree on both (a bare integer is epoch seconds to each).
	let recent = base.join("recent");
	gta(repo_s, &["worktree", "add", recent.to_str().unwrap()], b"");
	std::fs::remove_dir_all(&recent).unwrap();
	set_mtime(&repo.join(".git/worktrees/recent/index"), "200101010000"); // ~2001

	assert_eq!(
		gta_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=500000000"]
		),
		git_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=500000000"]
		),
	);
	assert!(
		gta_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=500000000"]
		)
		.is_empty(),
		"index newer than the 1985 cutoff is kept",
	);
	assert_eq!(
		sorted_lines(&gta_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=1500000000"],
		)),
		sorted_lines(&git_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=1500000000"],
		)),
	);
	assert!(
		gta_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=1500000000"],
		)
		.contains("worktrees/recent"),
		"index older than the 2017 cutoff is pruned",
	);

	// `--expire=all` (and `now`) prune every stale worktree regardless of index age — git maps them to
	// an infinite cutoff, not the current time — while `never` keeps a stale worktree that still has an
	// index. Verified against git on a stale worktree whose index mtime is ~now.
	let fresh = base.join("fresh");
	gta(repo_s, &["worktree", "add", fresh.to_str().unwrap()], b"");
	std::fs::remove_dir_all(&fresh).unwrap();
	assert_eq!(
		sorted_lines(&gta_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=all"]
		)),
		sorted_lines(&git_stderr(
			repo_s,
			&["worktree", "prune", "-n", "-v", "--expire=all"]
		)),
	);
	assert!(
		gta_stderr(repo_s, &["worktree", "prune", "-n", "-v", "--expire=all"])
			.contains("worktrees/fresh"),
		"a fresh-index stale worktree is still pruned by --expire=all",
	);
	assert_eq!(
		gta_stderr(repo_s, &["worktree", "prune", "-n", "-v", "--expire=never"]),
		git_stderr(repo_s, &["worktree", "prune", "-n", "-v", "--expire=never"]),
	);
	assert!(
		gta_stderr(repo_s, &["worktree", "prune", "-n", "-v", "--expire=never"]).is_empty(),
		"--expire=never keeps every stale worktree that still has an index",
	);

	// A small bare integer is not an epoch timestamp to git's approxidate (which parses it fuzzily and
	// non-monotonically). Rather than silently mis-dating it — parsing `0` as literal epoch 0 would
	// behave like `never` — gta rejects it with a clear error.
	assert!(
		gta_fail(repo_s, &["worktree", "prune", "--expire=0"]).contains("unsupported expiry time"),
	);

	std::fs::remove_dir_all(&base).ok();
}

/// A malformed admin entry that is a plain file — not a directory — is pruned and *unlinked* (git:
/// "not a valid directory"). `prune` is the cleanup path for exactly such corruption, so it must not
/// choke trying to `remove_dir_all` a file.
#[test]
fn prune_removes_a_non_directory_admin_entry() {
	let base = unique_tmp("gta-wt-junk");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let junk = repo.join(".git/worktrees/junk");
	std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
	std::fs::write(&junk, b"garbage").unwrap();

	let report = gta_stderr(repo_s, &["worktree", "prune", "-v"]);
	assert!(
		report.contains("Removing worktrees/junk: not a valid directory"),
		"unexpected prune report: {report:?}",
	);
	assert!(!junk.exists(), "the stray file entry was unlinked");

	std::fs::remove_dir_all(&base).ok();
}

/// `remove` resolves a worktree by a bare name (git's `find_worktree`), not only by an explicit path —
/// exercising the resolution retrofit shared with `lock`/`unlock`.
#[test]
fn remove_resolves_a_worktree_by_name() {
	let base = unique_tmp("gta-wt-name");
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(base_s, &["init", "--object-format=sha1", repo_s], b"");
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let solo = base.join("solo");
	gta(repo_s, &["worktree", "add", solo.to_str().unwrap()], b"");
	assert!(repo.join(".git/worktrees/solo").exists());

	gta(repo_s, &["worktree", "remove", "solo"], b"");
	assert!(!repo.join(".git/worktrees/solo").exists());
	assert!(!solo.exists());

	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn moves_worktrees_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_move("sha256");
}

#[test]
fn moves_worktrees_sha1() {
	check_move("sha1");
}

/// `worktree move` matches git: it relocates the checkout, repoints the admin `gitdir` at the new
/// `.git` file (leaving the checkout's own `.git` file pointing back at the unmoved admin dir), moves
/// *into* an existing directory under the source basename, refuses an occupied destination and the main
/// worktree, and needs a second `-f` to move a locked worktree. The moved worktree is byte-for-byte
/// git's layout, so git reads and lists it afterwards.
fn check_move(object_format: &str) {
	let base = unique_tmp(&format!("gta-wt-move-{object_format}"));
	let base_s = base.to_str().unwrap();
	let repo = base.join("repo");
	let repo_s = repo.to_str().unwrap();

	gta(
		base_s,
		&["init", &format!("--object-format={object_format}"), repo_s],
		b"",
	);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let wt = base.join("wt");
	gta(repo_s, &["worktree", "add", wt.to_str().unwrap()], b"");

	// A plain rename: the admin `gitdir` repoints at the new checkout, the checkout's `.git` still
	// points back at the (unmoved) admin dir, and git then reads the relocated worktree.
	let moved = base.join("moved");
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			wt.to_str().unwrap(),
			moved.to_str().unwrap(),
		],
		b"",
	);
	assert!(!wt.exists());
	assert!(moved.join("f.txt").is_file());
	assert_eq!(
		std::fs::read_to_string(repo.join(".git/worktrees/wt/gitdir")).unwrap(),
		format!("{}\n", real(&moved.join(".git"))),
	);
	assert_eq!(
		std::fs::read_to_string(moved.join(".git")).unwrap(),
		format!("gitdir: {}\n", real(&repo.join(".git/worktrees/wt"))),
	);
	// git reads what gta wrote: `list --porcelain` from both is byte-for-byte identical.
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// Moving into an existing directory drops the checkout inside it under its own basename (git's
	// `mv`-like rule), exactly as git resolves the destination.
	let into = base.join("into");
	std::fs::create_dir(&into).unwrap();
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			moved.to_str().unwrap(),
			into.to_str().unwrap(),
		],
		b"",
	);
	assert!(into.join("moved/f.txt").is_file());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// An occupied (non-empty) destination is refused, as is the main worktree. Moving into a directory
	// whose computed target (`<dir>/<basename>`) already exists non-empty collides, matching git.
	let occupied = base.join("occupied");
	std::fs::create_dir_all(occupied.join("moved")).unwrap();
	std::fs::write(occupied.join("moved/x"), "x").unwrap();
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"move",
				into.join("moved").to_str().unwrap(),
				occupied.to_str().unwrap(),
			],
		)
		.contains("already exists"),
	);
	assert!(
		gta_fail(
			repo_s,
			&["worktree", "move", ".", occupied.to_str().unwrap()]
		)
		.contains("is a main working tree"),
	);

	// A locked worktree needs two `-f`; one is not enough, matching git.
	let cur = into.join("moved");
	gta(repo_s, &["worktree", "lock", cur.to_str().unwrap()], b"");
	let dest1 = base.join("dest1");
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"move",
				"-f",
				cur.to_str().unwrap(),
				dest1.to_str().unwrap()
			],
		)
		.contains("locked working tree"),
	);
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			"-ff",
			cur.to_str().unwrap(),
			dest1.to_str().unwrap(),
		],
		b"",
	);
	assert!(dest1.join("f.txt").is_file());
	// The lock travels with the worktree (git leaves it locked after a forced move).
	assert!(repo.join(".git/worktrees/wt/locked").exists());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// A second worktree whose checkout is deleted leaves a stale registration. Moving another worktree
	// onto that path is refused without `-f`; with `-f`, git (and gta) drop the stale admin entry rather
	// than leave two admin dirs for one path — so `list` never reports a duplicate. (Unlock the mover
	// first, so the refusal is the registration check, not the lock guard.)
	gta(
		repo_s,
		&["worktree", "unlock", dest1.to_str().unwrap()],
		b"",
	);
	let ghost = base.join("ghost");
	gta(repo_s, &["worktree", "add", ghost.to_str().unwrap()], b"");
	std::fs::remove_dir_all(&ghost).unwrap();
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"move",
				dest1.to_str().unwrap(),
				ghost.to_str().unwrap()
			],
		)
		.contains("already registered"),
	);
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			"-f",
			dest1.to_str().unwrap(),
			ghost.to_str().unwrap(),
		],
		b"",
	);
	assert!(!repo.join(".git/worktrees/ghost").exists());
	assert!(ghost.join("f.txt").is_file());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// A stale registration that is *locked* is protected further: a single `-f` is refused (git's
	// distinct "missing but locked" message), and only `-f -f` overrides it and drops the locked entry.
	let locky = base.join("locky");
	gta(repo_s, &["worktree", "add", locky.to_str().unwrap()], b"");
	gta(repo_s, &["worktree", "lock", locky.to_str().unwrap()], b"");
	std::fs::remove_dir_all(&locky).unwrap();
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"move",
				"-f",
				ghost.to_str().unwrap(),
				locky.to_str().unwrap()
			],
		)
		.contains("missing but locked"),
	);
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			"-ff",
			ghost.to_str().unwrap(),
			locky.to_str().unwrap(),
		],
		b"",
	);
	assert!(!repo.join(".git/worktrees/locky").exists());
	assert!(locky.join("f.txt").is_file());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	std::fs::remove_dir_all(&base).ok();
}

/// `worktree move` on a git-created `worktree.useRelativePaths` worktree preserves the relative pointers
/// (the whole point of that mode — the tree can be relocated as a unit), rewriting both the admin
/// `gitdir` and the checkout's own `.git` for the new depth, byte-for-byte as git does, so git still
/// operates in the moved worktree.
#[test]
fn move_preserves_relative_path_pointers() {
	if !git_supports_relative_worktrees() {
		eprintln!("skipping: git without worktree.useRelativePaths");
		return;
	}
	let base = unique_tmp("gta-wt-move-rel");
	let repo = base.join("main");
	let repo_s = repo.to_str().unwrap();

	std::fs::create_dir_all(&repo).unwrap();
	git(repo_s, &["init", "-q", "."]);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	git(repo_s, &["config", "worktree.useRelativePaths", "true"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	git(repo_s, &["add", "."]);
	git(repo_s, &["commit", "-q", "-m", "base"]);

	let wt = base.join("wt");
	git(
		repo_s,
		&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
	);
	// git records relative pointers under this mode.
	assert!(
		Path::new(
			std::fs::read_to_string(wt.join(".git"))
				.unwrap()
				.trim_start_matches("gitdir: ")
				.trim()
		)
		.is_relative()
	);

	// Move the checkout a directory deeper, so a relative pointer must be recomputed.
	let sub = base.join("sub");
	std::fs::create_dir(&sub).unwrap();
	let moved = sub.join("moved");
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			wt.to_str().unwrap(),
			moved.to_str().unwrap(),
		],
		b"",
	);
	// Both pointers stay relative and resolve correctly — git reads and operates in the moved worktree.
	assert!(
		Path::new(
			std::fs::read_to_string(moved.join(".git"))
				.unwrap()
				.trim_start_matches("gitdir: ")
				.trim()
		)
		.is_relative()
	);
	assert!(
		Path::new(
			std::fs::read_to_string(repo.join(".git/worktrees/wt/gitdir"))
				.unwrap()
				.trim()
		)
		.is_relative()
	);
	assert_eq!(
		git(
			moved.to_str().unwrap(),
			&["rev-parse", "--abbrev-ref", "HEAD"]
		)
		.trim(),
		"feat",
	);
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	std::fs::remove_dir_all(&base).ok();
}

/// gitana has no submodule support, so — like git — it refuses to `move` or `remove` a worktree holding
/// an *initialized* submodule (whose `.git` link would otherwise be left dangling / orphaned), while
/// still allowing a worktree whose submodule is only registered, not checked out. `move` refuses even
/// with force; `remove` is overridden by a single `--force`, matching git. Skipped where local-file
/// submodules cannot be set up (older git, or `protocol.file` blocked).
#[test]
fn move_and_remove_refuse_a_worktree_with_an_initialized_submodule() {
	let base = unique_tmp("gta-wt-move-submod");
	let sub = base.join("sub");
	let sub_s = sub.to_str().unwrap();
	let repo = base.join("main");
	let repo_s = repo.to_str().unwrap();

	// A tiny upstream for the submodule, then a superproject embedding it.
	std::fs::create_dir_all(&sub).unwrap();
	git(sub_s, &["init", "-q", "."]);
	git(sub_s, &["config", "user.name", "T"]);
	git(sub_s, &["config", "user.email", "t@e"]);
	std::fs::write(sub.join("s.txt"), "s\n").unwrap();
	git(sub_s, &["add", "."]);
	git(sub_s, &["commit", "-q", "-m", "s"]);

	std::fs::create_dir_all(&repo).unwrap();
	git(repo_s, &["init", "-q", "."]);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	git(repo_s, &["add", "."]);
	git(repo_s, &["commit", "-q", "-m", "base"]);
	// Local-file submodules need `protocol.file.allow=always` on recent git; bail out of the test if the
	// environment refuses (the submodule working tree won't be initialized).
	let added = run_ok(&[
		"-C",
		repo_s,
		"-c",
		"protocol.file.allow=always",
		"submodule",
		"add",
		"-q",
		sub_s,
		"sub",
	]);
	if added.is_none() {
		eprintln!("skipping: local-file submodules unavailable");
		std::fs::remove_dir_all(&base).ok();
		return;
	}
	git(repo_s, &["commit", "-q", "-m", "submodule"]);

	// A worktree whose submodule is only *registered* (empty dir) may move — as git allows.
	let wt = base.join("wt");
	git(repo_s, &["worktree", "add", "-q", wt.to_str().unwrap()]);
	let moved = base.join("moved");
	gta(
		repo_s,
		&[
			"worktree",
			"move",
			wt.to_str().unwrap(),
			moved.to_str().unwrap(),
		],
		b"",
	);
	assert!(moved.join("f.txt").is_file());

	// Initialize the submodule in the moved worktree; now a move must be refused, byte-for-byte git's
	// message, and the worktree is left untouched.
	if run_ok(&[
		"-C",
		moved.to_str().unwrap(),
		"-c",
		"protocol.file.allow=always",
		"submodule",
		"update",
		"--init",
		"-q",
	])
	.is_none()
		|| !moved.join("sub/.git").exists()
	{
		eprintln!("skipping: submodule init unavailable");
		std::fs::remove_dir_all(&base).ok();
		return;
	}
	let err = gta_fail(
		repo_s,
		&[
			"worktree",
			"move",
			moved.to_str().unwrap(),
			base.join("moved2").to_str().unwrap(),
		],
	);
	assert!(err.contains("working trees containing submodules cannot be moved or removed"));
	assert!(moved.join("f.txt").is_file()); // untouched
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// The refusal does not rely on the working-copy `.gitmodules`: delete it (an unstaged change) and the
	// move is still refused, detected via the absorbed submodule git dir under the admin — as git does.
	std::fs::remove_file(moved.join(".gitmodules")).unwrap();
	assert!(
		gta_fail(
			repo_s,
			&[
				"worktree",
				"move",
				moved.to_str().unwrap(),
				base.join("moved3").to_str().unwrap()
			],
		)
		.contains("working trees containing submodules cannot be moved or removed"),
	);
	assert!(moved.join("f.txt").is_file());

	// `remove` guards the same way, but — matching git — a single `--force` overrides it (whereas `move`
	// refuses even with force). A plain `remove` is refused; `--force` deletes the worktree.
	assert!(
		gta_fail(repo_s, &["worktree", "remove", moved.to_str().unwrap()])
			.contains("working trees containing submodules cannot be moved or removed"),
	);
	assert!(moved.join("f.txt").is_file()); // untouched by the refusal
	gta(
		repo_s,
		&["worktree", "remove", "--force", moved.to_str().unwrap()],
		b"",
	);
	assert!(!moved.exists());
	// The admin dir keeps its original basename (`wt`) across the move; `remove` clears it.
	assert!(!repo.join(".git/worktrees/wt").exists());

	std::fs::remove_dir_all(&base).ok();
}

/// `worktree repair` on a `worktree.useRelativePaths` worktree whose checkout was moved by hand to a new
/// depth reconciles *both* pointers (the checkout's relative `.git` is stale at the new depth, so it too
/// must be recomputed), matching git — where a stale relative pointer would otherwise leave the worktree
/// unusable even after a successful-looking repair.
#[test]
fn repair_reconciles_relative_pointers_after_a_depth_change() {
	if !git_supports_relative_worktrees() {
		eprintln!("skipping: git without worktree.useRelativePaths");
		return;
	}
	let base = unique_tmp("gta-wt-repair-rel");
	let repo = base.join("main");
	let repo_s = repo.to_str().unwrap();

	std::fs::create_dir_all(&repo).unwrap();
	git(repo_s, &["init", "-q", "."]);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	git(repo_s, &["config", "worktree.useRelativePaths", "true"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	git(repo_s, &["add", "."]);
	git(repo_s, &["commit", "-q", "-m", "base"]);

	let wt = base.join("wt");
	git(
		repo_s,
		&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
	);

	// Move the checkout a level deeper by hand: its relative `.git` pointer (and the admin backlink) are
	// now both stale for the new depth.
	let deep = base.join("deep");
	std::fs::create_dir(&deep).unwrap();
	let moved = deep.join("wt");
	std::fs::rename(&wt, &moved).unwrap();

	gta(
		repo_s,
		&["worktree", "repair", moved.to_str().unwrap()],
		b"",
	);

	// Both pointers are recomputed (staying relative) and resolve — git reads the repaired worktree.
	assert!(
		Path::new(
			std::fs::read_to_string(moved.join(".git"))
				.unwrap()
				.trim_start_matches("gitdir: ")
				.trim()
		)
		.is_relative()
	);
	assert_eq!(
		git(
			moved.to_str().unwrap(),
			&["rev-parse", "--abbrev-ref", "HEAD"]
		)
		.trim(),
		"feat",
	);
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);
	// A second repair is a clean no-op now that both pointers are consistent.
	assert!(gta_stderr(repo_s, &["worktree", "repair", moved.to_str().unwrap()]).is_empty());

	// Deleting the checkout `.git` entirely: repair recreates it in *relative* form (inferred from the
	// still-relative admin backlink), keeping the worktree relocatable — as git does.
	std::fs::remove_file(moved.join(".git")).unwrap();
	gta_stderr(repo_s, &["worktree", "repair", moved.to_str().unwrap()]);
	assert!(
		Path::new(
			std::fs::read_to_string(moved.join(".git"))
				.unwrap()
				.trim_start_matches("gitdir: ")
				.trim()
		)
		.is_relative()
	);
	assert_eq!(
		git(
			moved.to_str().unwrap(),
			&["rev-parse", "--abbrev-ref", "HEAD"]
		)
		.trim(),
		"feat",
	);

	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn repairs_worktrees_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	check_repair("sha256");
}

#[test]
fn repairs_worktrees_sha1() {
	check_repair("sha1");
}

/// `worktree repair` matches git in both directions. When a checkout is moved by hand, `repair
/// <new-path>` fixes the admin `gitdir` backlink (git's `repair: gitdir incorrect` line). When the main
/// worktree is moved, a no-arg `repair` fixes each linked checkout's `.git` file (git's `repair: .git
/// file broken` line). After each repair git reads the reconciled layout.
fn check_repair(object_format: &str) {
	let base = unique_tmp(&format!("gta-wt-repair-{object_format}"));
	let base_s = base.to_str().unwrap();
	let repo = base.join("main");
	let repo_s = repo.to_str().unwrap();

	gta(
		base_s,
		&["init", &format!("--object-format={object_format}"), repo_s],
		b"",
	);
	git(repo_s, &["config", "user.name", "T"]);
	git(repo_s, &["config", "user.email", "t@e"]);
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	gta(repo_s, &["add", "."], b"");
	gta(repo_s, &["commit", "-m", "base"], b"");

	let wt = base.join("wt");
	gta(repo_s, &["worktree", "add", wt.to_str().unwrap()], b"");

	// Move the checkout by hand: the admin `gitdir` backlink is now stale. `repair <new-path>` fixes it
	// and reports the same `gitdir incorrect` line git does.
	let reloc = base.join("reloc");
	std::fs::rename(&wt, &reloc).unwrap();
	let report = gta_stderr(repo_s, &["worktree", "repair", reloc.to_str().unwrap()]);
	assert_eq!(
		report.trim(),
		format!(
			"repair: gitdir incorrect: {}",
			real(&repo.join(".git/worktrees/wt/gitdir"))
		),
	);
	assert_eq!(
		std::fs::read_to_string(repo.join(".git/worktrees/wt/gitdir")).unwrap(),
		format!("{}\n", real(&reloc.join(".git"))),
	);
	// A second repair is a no-op (nothing left to fix), like git.
	assert!(gta_stderr(repo_s, &["worktree", "repair", reloc.to_str().unwrap()]).is_empty());
	assert_eq!(
		gta(repo_s, &["worktree", "list", "--porcelain"], b""),
		git(repo_s, &["worktree", "list", "--porcelain"]),
	);

	// Move the main worktree by hand: each linked checkout's `.git` file now points at the old admin
	// path. A no-arg `repair` from the moved main rewrites them, reporting `. git file broken`.
	let main2 = base.join("main2");
	std::fs::rename(&repo, &main2).unwrap();
	let main2_s = main2.to_str().unwrap();
	let report = gta_stderr(main2_s, &["worktree", "repair"]);
	assert_eq!(
		report.trim(),
		format!("repair: .git file broken: {}", real(&reloc)),
	);
	assert_eq!(
		std::fs::read_to_string(reloc.join(".git")).unwrap(),
		format!("gitdir: {}\n", real(&main2.join(".git/worktrees/wt"))),
	);
	assert_eq!(
		gta(main2_s, &["worktree", "list", "--porcelain"], b""),
		git(main2_s, &["worktree", "list", "--porcelain"]),
	);

	// A no-arg repair run from a *subdirectory* of a hand-moved checkout still repairs it: the default
	// target is the discovered worktree root, not the raw cwd.
	std::fs::create_dir(reloc.join("sub")).unwrap();
	let reloc2 = base.join("reloc2");
	std::fs::rename(&reloc, &reloc2).unwrap();
	gta_stderr(
		reloc2.join("sub").to_str().unwrap(),
		&["worktree", "repair"],
	);
	assert_eq!(
		std::fs::read_to_string(main2.join(".git/worktrees/wt/gitdir")).unwrap(),
		format!("{}\n", real(&reloc2.join(".git"))),
	);
	assert_eq!(
		gta(main2_s, &["worktree", "list", "--porcelain"], b""),
		git(main2_s, &["worktree", "list", "--porcelain"]),
	);

	// An explicit repair path that is not a worktree is an error, not a silent success — as git treats it.
	assert!(
		gta_fail(main2_s, &["worktree", "repair", "/no/such/path/xyz"]).contains("not a valid path"),
	);
	// An explicit *main* worktree path is accepted (git does), a no-op here since its links are healthy.
	gta(main2_s, &["worktree", "repair", main2_s], b"");
	assert_eq!(
		gta(main2_s, &["worktree", "list", "--porcelain"], b""),
		git(main2_s, &["worktree", "list", "--porcelain"]),
	);

	// A checkout whose `.git` file is deleted but which the admin still registers is recreated by an
	// explicit `repair <path>` (via the admin backlink), not rejected — matching git.
	std::fs::remove_file(reloc2.join(".git")).unwrap();
	gta_stderr(main2_s, &["worktree", "repair", reloc2.to_str().unwrap()]);
	assert_eq!(
		std::fs::read_to_string(reloc2.join(".git")).unwrap(),
		format!("gitdir: {}\n", real(&main2.join(".git/worktrees/wt"))),
	);
	assert_eq!(
		gta(main2_s, &["worktree", "list", "--porcelain"], b""),
		git(main2_s, &["worktree", "list", "--porcelain"]),
	);

	std::fs::remove_dir_all(&base).ok();
}

/// `gta worktree list` matches stock git for a `--separate-git-dir` repository, where the git
/// directory lives apart from the checkout. git reports the main worktree as the git-dir path — its
/// `get_main_worktree` strips a trailing `/.git` from the common dir, ignoring the real work tree and
/// `core.worktree` — and the answer is the same whether listed from the main worktree or a linked
/// one. Regression guard: the repository-discovery extraction must not reintroduce a vantage-dependent
/// main-worktree path.
#[test]
fn worktree_list_matches_git_for_a_separate_git_dir() {
	let base = unique_tmp("gta-worktree-sgd");
	let base_s = base.to_str().unwrap();
	let gd = base.join("gd");
	let work = base.join("work");
	let linked = base.join("linked");
	let work_s = work.to_str().unwrap();
	let linked_s = linked.to_str().unwrap();

	// A repo whose git directory is separate from its checkout, plus a linked worktree.
	git(
		base_s,
		&[
			"init",
			"-q",
			&format!("--separate-git-dir={}", gd.display()),
			work_s,
		],
	);
	git(work_s, &["config", "user.name", "T"]);
	git(work_s, &["config", "user.email", "t@e"]);
	git(work_s, &["commit", "-q", "--allow-empty", "-m", "base"]);
	git(work_s, &["worktree", "add", "-q", linked_s, "-b", "lk"]);

	// The listed paths must match git's, and be the same from every vantage (main and linked).
	for vantage in [work_s, linked_s] {
		let want = worktree_paths(&git(vantage, &["worktree", "list"]));
		let got = worktree_paths(&gta(vantage, &["worktree", "list"], b""));
		assert_eq!(got, want, "worktree paths differ from vantage {vantage}");
	}

	std::fs::remove_dir_all(&base).ok();
}

/// The leading path column of each `worktree list` line.
fn worktree_paths(listing: &str) -> Vec<String> {
	listing
		.lines()
		.filter_map(|line| line.split_whitespace().next().map(str::to_owned))
		.collect()
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

/// Run `gta` expecting a non-zero exit; return its stderr.
fn gta_fail(dir: &str, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(!out.status.success(), "gta {args:?} unexpectedly succeeded");
	String::from_utf8(out.stderr).expect("gta stderr utf8")
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

/// Run `gta` expecting success; return its stderr (`worktree prune -v` reports there, like git).
fn gta_stderr(dir: &str, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stderr).expect("gta stderr utf8")
}

/// Run `git` expecting success; return its stderr.
fn git_stderr(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stderr).expect("git stderr utf8")
}

/// The lines of a prune report, sorted — so a multi-worktree comparison is independent of the
/// readdir order git walks in.
fn sorted_lines(text: &str) -> Vec<String> {
	let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
	lines.sort();
	lines
}

/// Set a file's mtime via `touch -t <[[CC]YY]MMDDhhmm>`, so a stale worktree's `index` gets a known
/// age for the `--expire` comparison.
fn set_mtime(path: &Path, stamp: &str) {
	let ok = Command::new("touch")
		.args(["-t", stamp, path.to_str().unwrap()])
		.status()
		.expect("run touch")
		.success();
	assert!(ok, "touch -t {stamp} {} failed", path.display());
}

/// A path's real (canonicalised) form as a string — resolves the `/var`→`/private/var` symlink the
/// macOS temp dir uses, so an expected pointer matches the realpath gta (and git) writes.
fn real(path: &Path) -> String {
	path.canonicalize().unwrap().display().to_string()
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-worktree");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}

/// Whether the `git` under test honours `worktree.useRelativePaths` (git 2.48+), so the relative-path
/// interop assertions can run — skipped gracefully on an older git.
fn git_supports_relative_worktrees() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-relwt");
		let repo = probe.join("main");
		let repo_s = repo.to_str().unwrap();
		std::fs::create_dir_all(&repo).unwrap();
		let ok = (|| {
			run_ok(&["init", "-q", repo_s])?;
			run_ok(&["-C", repo_s, "config", "user.name", "T"])?;
			run_ok(&["-C", repo_s, "config", "user.email", "t@e"])?;
			run_ok(&["-C", repo_s, "config", "worktree.useRelativePaths", "true"])?;
			std::fs::write(repo.join("f.txt"), "x\n").ok()?;
			run_ok(&["-C", repo_s, "add", "."])?;
			run_ok(&["-C", repo_s, "commit", "-q", "-m", "i"])?;
			run_ok(&[
				"-C",
				repo_s,
				"worktree",
				"add",
				"-q",
				probe.join("wt").to_str().unwrap(),
			])?;
			// The relative-paths mode is honoured only when git writes a *relative* gitdir pointer.
			let dotgit = std::fs::read_to_string(probe.join("wt").join(".git")).ok()?;
			let pointer = dotgit.trim().strip_prefix("gitdir:")?.trim();
			Some(Path::new(pointer).is_relative())
		})()
		.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}

/// Run `git` with `args`, returning `Some(())` on success — a terse helper for capability probes.
fn run_ok(args: &[&str]) -> Option<()> {
	Command::new("git")
		.args(args)
		.output()
		.ok()
		.filter(|out| out.status.success())
		.map(|_| ())
}
