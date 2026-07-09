//! `gta` operating inside a **linked worktree** (`git worktree add`), where `.git` is a file and
//! git splits the repository between a per-worktree directory (`HEAD`, `index`) and a shared common
//! directory (`objects`, `refs`, `config`). gta must read `HEAD` from the worktree but objects and
//! branch refs from the common dir, and a commit must advance only that worktree's branch.
//!
//! Cross-checked against stock `git` so the routing matches git's own behaviour.

use std::path::PathBuf;
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
