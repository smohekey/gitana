#![cfg(unix)]
//! `WorkTree::commit`, `switch_branch` and `reset`, `Repository::create_branch`, and `add`'s refusal of
//! a nested repository, checked against stock git where git has an answer.

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore, WorktreeFileStore};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::{BranchError, BranchName, Operation, Repository};
use gitana_worktree::{
	CommitError, ResetError, ResetMode, SwitchError, SwitchOptions, WorkTree, WorktreeError,
};

type Tree = WorkTree<LocalFileStore, CapWorkDir, Sha256>;

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &Path) -> Tree {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
}

/// An identity with fixed lines.
struct TestIdentity;

impl gitana_identity::Identity for TestIdentity {
	async fn author(&self) -> anyhow::Result<String> {
		Ok("A U Thor <author@example.com> 1700000000 +0000".to_owned())
	}
	async fn committer(&self) -> anyhow::Result<String> {
		Ok("C O Mitter <committer@example.com> 1700000000 +0000".to_owned())
	}
	async fn committer_or_default(&self) -> anyhow::Result<String> {
		self.committer().await
	}
}

/// A repository at a new temporary folder with one commit (`a.txt`) on `main`; `None` where git
/// can't make sha256 repositories.
fn repository(tag: &str) -> Option<(PathBuf, String)> {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return None;
	}
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap().to_owned();
	git(&["init", "--object-format=sha256", "-q", "-b", "main", &w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", &w, "add", "."]);
	commit(&w, "one");
	Some((work, w))
}

/// A merge left in conflict on `a.txt`, from a branch `side`.
fn conflicted_merge(w: &str, work: &Path) {
	git(&["-C", w, "switch", "-q", "-c", "side"]);
	std::fs::write(work.join("a.txt"), b"side\n").unwrap();
	commit_all(w, "side");
	git(&["-C", w, "switch", "-q", "main"]);
	std::fs::write(work.join("a.txt"), b"main\n").unwrap();
	commit_all(w, "main");
	let _ = Command::new("git")
		.args([
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
			"merge",
			"side",
		])
		.output()
		.unwrap();
	assert!(work.join(".git/MERGE_HEAD").exists());
}

#[tokio::test]
async fn commit_records_what_is_staged_and_refuses_while_a_merge_is_in_progress() {
	let Some((work, w)) = repository("commit") else {
		return;
	};
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	git(&["-C", &w, "add", "b.txt"]);
	let wt = make_repo(&work);
	let id = wt.commit("Two", &TestIdentity).await.unwrap();
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]).trim(), id.to_hex());
	assert_eq!(
		git(&["-C", &w, "log", "-1", "--format=%an <%ae>|%s"]).trim(),
		"A U Thor <author@example.com>|Two"
	);
	// Nothing staged now: nothing to commit.
	assert!(matches!(
		wt.commit("Again", &TestIdentity).await,
		Err(CommitError::NothingToCommit)
	));
	// Mid-merge: refused before anything else, the merge left as it is.
	conflicted_merge(&w, &work);
	let wt = make_repo(&work);
	assert!(matches!(
		wt.commit("Mid-merge", &TestIdentity).await,
		Err(CommitError::InProgress(Operation::Merge))
	));
	assert!(work.join(".git/MERGE_HEAD").exists());
}

/// As git: a file only meant to be added (`git add -N`) shows in the work-tree column alone, ` A`
/// whether or not it's been edited since, and ` D` once it's gone; one that HEAD has, taken out of
/// the index and meant to be added again, is a staged deletion too, `DA`.
#[tokio::test]
async fn status_shows_intent_to_add_files_as_git_does() {
	let Some((work, w)) = repository("status-intent") else {
		return;
	};
	for (name, text) in [("new.txt", "N\n"), ("edit.txt", "E\n"), ("gone.txt", "G\n")] {
		std::fs::write(work.join(name), text).unwrap();
	}
	git(&["-C", &w, "add", "-N", "new.txt", "edit.txt", "gone.txt"]);
	git(&["-C", &w, "rm", "-q", "--cached", "a.txt"]);
	git(&["-C", &w, "add", "-N", "a.txt"]);
	std::fs::write(work.join("edit.txt"), b"E2\n").unwrap();
	std::fs::remove_file(work.join("gone.txt")).unwrap();
	let status = make_repo(&work).status(None).await.unwrap();
	let shown: Vec<String> = status
		.changed
		.iter()
		.map(|entry| format!("{}{} {}", entry.index, entry.worktree, entry.path))
		.collect();
	assert_eq!(
		shown,
		["DA a.txt", " A edit.txt", " D gone.txt", " A new.txt"]
	);
	assert_eq!(
		git(&["-C", &w, "status", "--porcelain"]),
		"DA a.txt\n A edit.txt\n D gone.txt\n A new.txt\n"
	);
}

/// As git: a commit leaves an intent-to-add entry out (its content was never staged), so with only
/// one there's nothing to commit; a commit deleting every file makes an empty tree; and an unborn
/// branch with an empty index has nothing to commit.
#[tokio::test]
async fn commit_leaves_intent_to_add_out_and_can_delete_every_file() {
	let Some((work, w)) = repository("commit-intent") else {
		return;
	};
	std::fs::write(work.join("new.txt"), b"N\n").unwrap();
	git(&["-C", &w, "add", "-N", "new.txt"]);
	let wt = make_repo(&work);
	assert!(matches!(
		wt.commit("Only intended", &TestIdentity).await,
		Err(CommitError::NothingToCommit)
	));
	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	git(&["-C", &w, "add", "a.txt"]);
	let wt = make_repo(&work);
	wt.commit("Two", &TestIdentity).await.unwrap();
	assert_eq!(
		git(&["-C", &w, "ls-tree", "--name-only", "HEAD"]).trim(),
		"a.txt"
	);
	git(&["-C", &w, "rm", "-q", "--cached", "new.txt"]);
	git(&["-C", &w, "rm", "-q", "a.txt"]);
	let wt = make_repo(&work);
	let gone = wt.commit("Gone", &TestIdentity).await.unwrap();
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]).trim(), gone.to_hex());
	assert_eq!(git(&["-C", &w, "ls-tree", "HEAD"]).trim(), "");
	assert!(matches!(
		wt.commit("Again", &TestIdentity).await,
		Err(CommitError::NothingToCommit)
	));
	let unborn = unique_tmp("commit-unborn");
	let u = unborn.to_str().unwrap().to_owned();
	git(&["init", "--object-format=sha256", "-q", "-b", "main", &u]);
	assert!(matches!(
		make_repo(&unborn).commit("Nothing", &TestIdentity).await,
		Err(CommitError::Empty)
	));
}

#[tokio::test]
async fn reset_moves_the_branch_and_resets_as_its_mode_says() {
	let Some((work, w)) = repository("reset-modes") else {
		return;
	};
	let first = git(&["-C", &w, "rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	commit_all(&w, "two");
	// Soft: the branch moves; the index and file keep the second commit's content.
	std::fs::write(work.join("a.txt"), b"A3\n").unwrap();
	let wt = make_repo(&work);
	let at = wt
		.reset("HEAD~1", ResetMode::Soft, &TestIdentity, None)
		.await
		.unwrap();
	assert_eq!(at.commit.to_hex(), first);
	assert_eq!(at.autostash, None);
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]).trim(), first);
	assert_eq!(
		git(&["-C", &w, "diff", "--cached", "--name-only"]).trim(),
		"a.txt"
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A3\n");
	assert_eq!(
		git(&["-C", &w, "reflog", "-1", "--format=%gs"]).trim(),
		"reset: moving to HEAD~1"
	);
	// Mixed: the index too, the file kept.
	wt.reset("HEAD", ResetMode::Mixed, &TestIdentity, None)
		.await
		.unwrap();
	assert_eq!(
		git(&["-C", &w, "diff", "--cached", "--name-only"]).trim(),
		""
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A3\n");
	// Hard: the file too.
	wt.reset("HEAD", ResetMode::Hard, &TestIdentity, None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A\n");
	// Not a commit git knows.
	assert!(matches!(
		wt.reset("nowhere", ResetMode::Mixed, &TestIdentity, None)
			.await,
		Err(ResetError::Repository(_))
	));
}

#[tokio::test]
async fn reset_treats_a_merge_in_progress_as_git_does() {
	let Some((work, w)) = repository("reset-merge") else {
		return;
	};
	conflicted_merge(&w, &work);
	let wt = make_repo(&work);
	// git: "Cannot do a soft reset in the middle of a merge."
	assert!(matches!(
		wt.reset("HEAD", ResetMode::Soft, &TestIdentity, None).await,
		Err(ResetError::MergeInProgress)
	));
	assert!(work.join(".git/MERGE_HEAD").exists());
	// Mixed abandons the merge, its state removed, as git's does.
	wt.reset("HEAD", ResetMode::Mixed, &TestIdentity, None)
		.await
		.unwrap();
	for state in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE", "AUTO_MERGE"] {
		assert!(!work.join(".git").join(state).exists(), "{state} left");
	}
	assert_eq!(
		git(&["-C", &w, "status", "--porcelain", "--untracked-files=no"]).trim(),
		"M a.txt"
	);
}

/// A merge's autostash (git's `merge --autostash`) is saved as the newest stash entry when a reset
/// abandons the merge, as git's reset saves it: listed by `git stash list` as "autostash", holding
/// the edits, with `MERGE_AUTOSTASH` gone. The stash's reflog is written even with
/// `core.logAllRefUpdates` off, as git forces it.
#[tokio::test]
async fn reset_saves_a_merge_s_autostash_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("reset-autostash-{tool}")) else {
			return;
		};
		std::fs::write(work.join("b.txt"), b"B\n").unwrap();
		commit_all(&w, "b");
		git(&["-C", &w, "config", "core.logAllRefUpdates", "false"]);
		git(&["-C", &w, "switch", "-q", "-c", "side"]);
		std::fs::write(work.join("a.txt"), b"side\n").unwrap();
		commit_all(&w, "side");
		git(&["-C", &w, "switch", "-q", "main"]);
		std::fs::write(work.join("a.txt"), b"main\n").unwrap();
		commit_all(&w, "main");
		std::fs::write(work.join("b.txt"), b"my edit\n").unwrap();
		let _ = Command::new("git")
			.args([
				"-C",
				&w,
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"-c",
				"core.hooksPath=/dev/null",
				"merge",
				"--autostash",
				"side",
			])
			.output()
			.unwrap();
		let pending = std::fs::read_to_string(work.join(".git/MERGE_AUTOSTASH"))
			.unwrap()
			.trim()
			.to_owned();
		if tool == "git" {
			git(&[
				"-C",
				&w,
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"reset",
				"-q",
				"--hard",
			]);
		} else {
			let outcome = make_repo(&work)
				.reset("HEAD", ResetMode::Hard, &TestIdentity, None)
				.await
				.unwrap();
			assert_eq!(
				outcome.autostash.map(|id| id.to_hex()),
				Some(pending.clone())
			);
		}
		for state in ["MERGE_AUTOSTASH", "MERGE_HEAD"] {
			assert!(
				!work.join(".git").join(state).exists(),
				"{tool}: {state} left"
			);
		}
		outcomes.push((
			git(&["-C", &w, "rev-parse", "refs/stash"]).trim() == pending,
			git(&["-C", &w, "stash", "list", "--format=%gs"]),
			git(&["-C", &w, "stash", "show", "-p", "--no-color"]),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert!(outcomes[1].0);
	assert_eq!(outcomes[1].1, "autostash\n");
}

/// A reset whose merge autostash can't be read (`MERGE_AUTOSTASH` holds no object id) stops before
/// it changes anything: the working tree, the index and `HEAD` are all as they were.
#[tokio::test]
async fn reset_with_an_unreadable_autostash_changes_nothing() {
	let Some((work, w)) = repository("reset-bad-autostash") else {
		return;
	};
	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	commit_all(&w, "two");
	let head = git(&["-C", &w, "rev-parse", "HEAD"]);
	std::fs::write(work.join(".git/MERGE_AUTOSTASH"), b"not an object id\n").unwrap();
	assert!(matches!(
		make_repo(&work)
			.reset("HEAD~1", ResetMode::Hard, &TestIdentity, None)
			.await,
		Err(ResetError::Repository(_))
	));
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");
	assert_eq!(git(&["-C", &w, "status", "--porcelain"]), "");
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]), head);
}

/// A merge's autostash is saved before `HEAD` moves, so a reset that can't save it (here
/// `refs/stash.lock` is held) changes nothing: `HEAD`, the merge in progress and its autostash are all
/// as they were, and no stash entry is made.
#[tokio::test]
async fn a_reset_that_cannot_save_the_autostash_changes_nothing() {
	let Some((work, w)) = repository("reset-stash-locked") else {
		return;
	};
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	commit_all(&w, "b");
	git(&["-C", &w, "switch", "-q", "-c", "side"]);
	std::fs::write(work.join("a.txt"), b"side\n").unwrap();
	commit_all(&w, "side");
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join("a.txt"), b"main\n").unwrap();
	commit_all(&w, "main");
	std::fs::write(work.join("b.txt"), b"my edit\n").unwrap();
	let _ = Command::new("git")
		.args([
			"-C",
			&w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
			"merge",
			"--autostash",
			"side",
		])
		.output()
		.unwrap();
	assert!(work.join(".git/MERGE_AUTOSTASH").exists());
	let head = git(&["-C", &w, "rev-parse", "HEAD"]);
	let parent = git(&["-C", &w, "rev-parse", "HEAD~1"]);
	std::fs::write(work.join(".git/refs/stash.lock"), b"").unwrap();
	let wt = make_repo(&work);
	let parent = gitana_object::ObjectId::<Sha256>::from_hex(parent.trim()).unwrap();
	assert!(
		wt.repository()
			.reset_head_abandoning_operations(
				parent,
				"C O Mitter <committer@example.com> 1700000000 +0000",
				"reset: moving to HEAD~1",
			)
			.await
			.is_err()
	);
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]), head);
	for state in ["MERGE_HEAD", "MERGE_MSG", "MERGE_AUTOSTASH"] {
		assert!(work.join(".git").join(state).exists(), "{state} gone");
	}
	assert_eq!(git(&["-C", &w, "stash", "list"]), "");
}

/// A hard reset prepares everything that can fail before it touches the index or working tree: with
/// the autostash unable to be saved (`refs/stash.lock` held), it stops with a staged merge resolution,
/// the files, `HEAD` and the merge all as they were.
#[tokio::test]
async fn a_hard_reset_that_cannot_save_the_autostash_keeps_the_resolution() {
	let Some((work, w)) = repository("reset-hard-stash-locked") else {
		return;
	};
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	commit_all(&w, "b");
	git(&["-C", &w, "switch", "-q", "-c", "side"]);
	std::fs::write(work.join("a.txt"), b"side\n").unwrap();
	commit_all(&w, "side");
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join("a.txt"), b"main\n").unwrap();
	commit_all(&w, "main");
	std::fs::write(work.join("b.txt"), b"my edit\n").unwrap();
	let _ = Command::new("git")
		.args([
			"-C",
			&w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
			"merge",
			"--autostash",
			"side",
		])
		.output()
		.unwrap();
	assert!(work.join(".git/MERGE_AUTOSTASH").exists());
	std::fs::write(work.join("a.txt"), b"resolved\n").unwrap();
	git(&["-C", &w, "add", "a.txt"]);
	let head = git(&["-C", &w, "rev-parse", "HEAD"]);
	let staged = git(&["-C", &w, "ls-files", "-s"]);
	std::fs::write(work.join(".git/refs/stash.lock"), b"").unwrap();
	assert!(
		make_repo(&work)
			.reset("HEAD~1", ResetMode::Hard, &TestIdentity, None)
			.await
			.is_err()
	);
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"resolved\n");
	assert_eq!(git(&["-C", &w, "ls-files", "-s"]), staged);
	assert_eq!(git(&["-C", &w, "rev-parse", "HEAD"]), head);
	for state in ["MERGE_HEAD", "MERGE_AUTOSTASH"] {
		assert!(work.join(".git").join(state).exists(), "{state} gone");
	}
}

#[tokio::test]
async fn create_branch_makes_one_at_a_commit_as_git_does() {
	let Some((work, w)) = repository("create-branch") else {
		return;
	};
	let wt = make_repo(&work);
	let repo = wt.repository();
	let head = git(&["-C", &w, "rev-parse", "HEAD"]).trim().to_owned();
	let topic = BranchName::parse("topic").unwrap();
	let at = repo
		.create_branch(&topic, None, &TestIdentity)
		.await
		.unwrap();
	assert_eq!(at.to_hex(), head);
	assert_eq!(
		git(&["-C", &w, "rev-parse", "refs/heads/topic"]).trim(),
		head
	);
	assert_eq!(
		git(&["-C", &w, "reflog", "-1", "--format=%gs", "topic"]).trim(),
		"branch: Created from main"
	);
	assert_eq!(git(&["-C", &w, "branch", "--show-current"]).trim(), "main");
	// A tag names the commit it tags.
	git(&[
		"-C",
		&w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"tag",
		"-a",
		"-m",
		"v",
		"v1",
	]);
	let tagged = BranchName::parse("tagged").unwrap();
	assert_eq!(
		repo
			.create_branch(&tagged, Some("v1"), &TestIdentity)
			.await
			.unwrap()
			.to_hex(),
		head
	);
	assert!(matches!(
		repo.create_branch(&topic, None, &TestIdentity).await,
		Err(BranchError::Exists(name)) if name == topic
	));
}

fn switch_options(overwrite_ignored: bool) -> SwitchOptions<'static> {
	SwitchOptions {
		overwrite_ignored,
		..SwitchOptions::default()
	}
}

#[tokio::test]
async fn switch_carries_local_work_and_refuses_what_it_would_overwrite() {
	let Some((work, w)) = repository("switch") else {
		return;
	};
	git(&["-C", &w, "branch", "other"]);
	git(&["-C", &w, "switch", "-q", "other"]);
	std::fs::write(work.join("o.txt"), b"other\n").unwrap();
	std::fs::write(work.join("a.txt"), b"A on other\n").unwrap();
	commit_all(&w, "on other");
	git(&["-C", &w, "switch", "-q", "main"]);
	// A local change other doesn't touch travels.
	std::fs::write(work.join("b.txt"), b"mine\n").unwrap();
	git(&["-C", &w, "add", "b.txt"]);
	let wt = make_repo(&work);
	let other = BranchName::parse("other").unwrap();
	wt.switch_branch(&other, &switch_options(true), &TestIdentity)
		.await
		.unwrap();
	assert_eq!(git(&["-C", &w, "branch", "--show-current"]).trim(), "other");
	assert_eq!(std::fs::read(work.join("b.txt")).unwrap(), b"mine\n");
	assert_eq!(
		git(&["-C", &w, "reflog", "-1", "--format=%gs"]).trim(),
		"checkout: moving from main to other"
	);
	// Back on main, a change to a.txt, which other changes, is refused.
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join("a.txt"), b"local\n").unwrap();
	let main_wt = make_repo(&work);
	assert!(matches!(
		main_wt
			.switch_branch(&other, &switch_options(true), &TestIdentity)
			.await,
		Err(SwitchError::Worktree(WorktreeError::Conflict(_)))
	));
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"local\n");
	assert_eq!(git(&["-C", &w, "branch", "--show-current"]).trim(), "main");
	// No such branch; one to make that's there already; one made, at a start.
	let nowhere = BranchName::parse("nowhere").unwrap();
	assert!(matches!(
		main_wt
			.switch_branch(&nowhere, &switch_options(true), &TestIdentity)
			.await,
		Err(SwitchError::NoSuchBranch(_))
	));
	git(&["-C", &w, "checkout", "-q", "--", "a.txt"]);
	let made = SwitchOptions {
		create: Some(None),
		..switch_options(true)
	};
	assert!(matches!(
		main_wt.switch_branch(&other, &made, &TestIdentity).await,
		Err(SwitchError::Exists(_))
	));
	let fresh = BranchName::parse("fresh").unwrap();
	main_wt
		.switch_branch(&fresh, &made, &TestIdentity)
		.await
		.unwrap();
	assert_eq!(git(&["-C", &w, "branch", "--show-current"]).trim(), "fresh");
}

#[tokio::test]
async fn switch_told_not_to_overwrite_ignored_files_refuses_where_git_would_overwrite_them() {
	let Some((work, w)) = repository("switch-ignored") else {
		return;
	};
	git(&["-C", &w, "switch", "-q", "-c", "other"]);
	std::fs::write(work.join(".env"), b"theirs\n").unwrap();
	git(&["-C", &w, "add", ".env"]);
	commit(&w, "env");
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join(".git/info/exclude"), b".env\n").unwrap();
	std::fs::write(work.join(".env"), b"mine\n").unwrap();
	let wt = make_repo(&work);
	let other = BranchName::parse("other").unwrap();
	match wt
		.switch_branch(&other, &switch_options(false), &TestIdentity)
		.await
	{
		Err(SwitchError::Worktree(WorktreeError::IgnoredOverwrite(path))) => {
			assert_eq!(path.to_string(), ".env");
		}
		other => panic!("not refused: {other:?}"),
	}
	assert_eq!(std::fs::read(work.join(".env")).unwrap(), b"mine\n");
	assert_eq!(git(&["-C", &w, "branch", "--show-current"]).trim(), "main");
	// As git does by default, overwritten.
	wt.switch_branch(&other, &switch_options(true), &TestIdentity)
		.await
		.unwrap();
	assert_eq!(std::fs::read(work.join(".env")).unwrap(), b"theirs\n");
}

/// Told not to overwrite ignored files, a switch looks for them only once it holds the index lock, on
/// the index it then checks out from, so no other writer can change the index between the two: with
/// `index.lock` already taken, it stops at the lock, before any look at an ignored file.
#[tokio::test]
async fn switch_checks_ignored_files_only_once_it_holds_the_index_lock() {
	let Some((work, w)) = repository("switch-ignored-locked") else {
		return;
	};
	git(&["-C", &w, "switch", "-q", "-c", "other"]);
	std::fs::write(work.join(".env"), b"theirs\n").unwrap();
	git(&["-C", &w, "add", ".env"]);
	commit(&w, "env");
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join(".git/info/exclude"), b".env\n").unwrap();
	std::fs::write(work.join(".env"), b"mine\n").unwrap();
	let other = BranchName::parse("other").unwrap();
	std::fs::write(work.join(".git/index.lock"), b"").unwrap();
	assert!(matches!(
		make_repo(&work)
			.switch_branch(&other, &switch_options(false), &TestIdentity)
			.await,
		Err(SwitchError::Worktree(WorktreeError::IndexLocked))
	));
	std::fs::remove_file(work.join(".git/index.lock")).unwrap();
	assert!(matches!(
		make_repo(&work)
			.switch_branch(&other, &switch_options(false), &TestIdentity)
			.await,
		Err(SwitchError::Worktree(WorktreeError::IgnoredOverwrite(_)))
	));
	assert_eq!(std::fs::read(work.join(".env")).unwrap(), b"mine\n");
}

#[tokio::test]
async fn switch_refuses_a_branch_checked_out_elsewhere_or_with_a_merge_in_progress() {
	let Some((work, w)) = repository("switch-elsewhere") else {
		return;
	};
	let linked = unique_tmp("switch-elsewhere-linked");
	std::fs::remove_dir_all(&linked).unwrap();
	git(&[
		"-C",
		&w,
		"worktree",
		"add",
		"-q",
		"-b",
		"taken",
		linked.to_str().unwrap(),
	]);
	let wt = make_repo(&work);
	let taken = BranchName::parse("taken").unwrap();
	match wt
		.switch_branch(&taken, &switch_options(true), &TestIdentity)
		.await
	{
		Err(SwitchError::CheckedOutElsewhere { branch, place }) => {
			assert_eq!(branch, taken);
			assert_eq!(
				Path::new(&place).canonicalize().unwrap(),
				linked.canonicalize().unwrap()
			);
		}
		other => panic!("not refused: {other:?}"),
	}
	conflicted_merge(&w, &work);
	git(&["-C", &w, "branch", "free"]);
	let free = BranchName::parse("free").unwrap();
	assert!(matches!(
		make_repo(&work)
			.switch_branch(&free, &switch_options(true), &TestIdentity)
			.await,
		Err(SwitchError::InProgress(Operation::Merge))
	));
}

/// A main repository whose git dir is `…/worktrees/repo` (made with `--separate-git-dir`), beside a
/// linked worktree whose id is also `repo`. Each is told from the other by its layout (only a linked
/// worktree's git dir holds `commondir`), as git tells them, not by its folder's name.
#[tokio::test]
async fn switch_tells_main_and_linked_worktrees_apart_by_layout_not_name() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let base = unique_tmp("switch-layout");
	let common = base.join("store/worktrees/repo");
	let main = base.join("main");
	let linked = base.join("repo");
	let (m, l) = (main.to_str().unwrap(), linked.to_str().unwrap());
	std::fs::create_dir_all(common.parent().unwrap()).unwrap();
	git(&[
		"init",
		"--object-format=sha256",
		"-q",
		"-b",
		"main",
		"--separate-git-dir",
		common.to_str().unwrap(),
		m,
	]);
	std::fs::write(main.join("a.txt"), b"A\n").unwrap();
	git(&["-C", m, "add", "."]);
	commit(m, "one");
	git(&["-C", m, "worktree", "add", "-q", "-b", "side", l]);
	let linked_git = common.join("worktrees/repo");
	assert!(linked_git.join("commondir").is_file());
	// Stock git refuses each a branch the other has checked out.
	assert!(git_refuses(&["-C", m, "switch", "side"]).contains("already used by worktree"));
	assert!(git_refuses(&["-C", l, "switch", "main"]).contains("already used by worktree"));

	let main_wt = WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&common),
		))),
		CapWorkDir::from_dir(open_dir(&main)),
		common.clone(),
	);
	let side = BranchName::parse("side").unwrap();
	match main_wt
		.switch_branch(&side, &switch_options(true), &TestIdentity)
		.await
	{
		Err(SwitchError::CheckedOutElsewhere { place, .. }) => assert_eq!(
			Path::new(&place).canonicalize().unwrap(),
			linked.canonicalize().unwrap()
		),
		other => panic!("not refused: {other:?}"),
	}
	// Its own branch isn't checked out elsewhere: switching to it changes nothing, as git's does.
	let main_branch = BranchName::parse("main").unwrap();
	main_wt
		.switch_branch(&main_branch, &switch_options(true), &TestIdentity)
		.await
		.unwrap();

	let linked_wt = WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(WorktreeFileStore::new(
			open_dir(&common),
			open_dir(&linked_git),
		))),
		CapWorkDir::from_dir(open_dir(&linked)),
		linked_git.clone(),
	);
	match linked_wt
		.switch_branch(&main_branch, &switch_options(true), &TestIdentity)
		.await
	{
		// Named as git names it: by the main worktree's git dir, which isn't a `.git` folder.
		Err(SwitchError::CheckedOutElsewhere { place, .. }) => assert_eq!(
			Path::new(&place).canonicalize().unwrap(),
			common.canonicalize().unwrap()
		),
		other => panic!("not refused: {other:?}"),
	}
	std::fs::remove_dir_all(&base).ok();
}

/// A linked worktree built from store capabilities alone, with no git folder path (as the wasm
/// component builds one), still tells the main worktree and its own admin entry apart: the main
/// worktree's branch is refused, and its own branch isn't counted as checked out elsewhere.
#[tokio::test]
async fn switch_from_a_linked_worktree_without_a_git_folder_path() {
	let Some((work, w)) = repository("switch-capability-only") else {
		return;
	};
	let linked = unique_tmp("switch-capability-only-linked");
	std::fs::remove_dir_all(&linked).unwrap();
	git(&[
		"-C",
		&w,
		"worktree",
		"add",
		"-q",
		"-b",
		"side",
		linked.to_str().unwrap(),
	]);
	git(&["-C", &w, "branch", "free"]);
	let admin = work
		.join(".git/worktrees")
		.join(linked.file_name().unwrap());
	let wt = WorkTree::new(
		Repository::new(ObjectStore::<_, Sha256>::new(WorktreeFileStore::new(
			open_dir(work.join(".git")),
			open_dir(&admin),
		))),
		CapWorkDir::from_dir(open_dir(&linked)),
		"",
	);
	let branch = |name: &str| BranchName::parse(name).unwrap();
	assert!(matches!(
		wt.switch_branch(&branch("main"), &switch_options(true), &TestIdentity)
			.await,
		Err(SwitchError::CheckedOutElsewhere { .. })
	));
	wt.switch_branch(&branch("side"), &switch_options(true), &TestIdentity)
		.await
		.unwrap();
	wt.switch_branch(&branch("free"), &switch_options(true), &TestIdentity)
		.await
		.unwrap();
	assert_eq!(
		git(&["-C", linked.to_str().unwrap(), "branch", "--show-current"]).trim(),
		"free"
	);
}

/// Told not to overwrite ignored files, a switch leaves alone an ignored file where the
/// sparse-checkout keeps the target's file out, as git's `switch --no-overwrite-ignore` does: the
/// checkout never writes there.
#[tokio::test]
async fn switch_keeps_an_ignored_file_outside_the_sparse_checkout_as_git_does() {
	let mut results = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-sparse-{tool}")) else {
			return;
		};
		std::fs::create_dir_all(work.join("in")).unwrap();
		std::fs::write(work.join("in/a"), b"i\n").unwrap();
		std::fs::write(work.join(".gitignore"), b".env\n").unwrap();
		git(&["-C", &w, "add", "."]);
		commit(&w, "in");
		git(&["-C", &w, "switch", "-q", "-c", "other"]);
		std::fs::create_dir_all(work.join("out")).unwrap();
		std::fs::write(work.join("out/.env"), b"tracked\n").unwrap();
		git(&["-C", &w, "add", "-f", "out/.env"]);
		commit(&w, "env");
		git(&["-C", &w, "switch", "-q", "main"]);
		git(&["-C", &w, "sparse-checkout", "set", "--cone", "in"]);
		std::fs::create_dir_all(work.join("out")).unwrap();
		std::fs::write(work.join("out/.env"), b"local\n").unwrap();
		if tool == "git" {
			git(&["-C", &w, "switch", "-q", "--no-overwrite-ignore", "other"]);
		} else {
			let other = BranchName::parse("other").unwrap();
			make_repo(&work)
				.switch_branch(&other, &switch_options(false), &TestIdentity)
				.await
				.unwrap();
		}
		results.push((
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join("out/.env")).unwrap(),
			git(&["-C", &w, "ls-files", "-t", "--", "out/.env"]),
		));
	}
	assert_eq!(results[1], results[0]);
	// The file is kept, and, being there, is tracked as checked out (git clears the skip-worktree bit
	// of a sparse entry whose file is present).
	assert_eq!(
		results[1],
		(
			"other\n".to_owned(),
			"local\n".to_owned(),
			"H out/.env\n".to_owned()
		)
	);
}

/// Told not to overwrite ignored files, a switch that puts a file where an ignored folder is refuses
/// only if the folder holds an untracked file (ignored, being in it), naming it, as git's
/// `switch --no-overwrite-ignore` refuses; a folder whose files are all tracked is replaced. The
/// folder may be ignored by its own rule or by one on a folder above it.
#[tokio::test]
async fn switch_replaces_an_ignored_folder_only_when_nothing_untracked_is_in_it() {
	let cases = [
		("folder/\n", "folder", None),
		("folder/\n", "folder", Some("folder/junk")),
		("outer/\n", "outer/folder", Some("outer/folder/junk")),
	];
	for (rule, folder, junk) in cases {
		let mut outcomes = Vec::new();
		for tool in ["git", "gitana"] {
			let Some((work, w)) = repository(&format!("switch-ignored-folder-{tool}")) else {
				return;
			};
			std::fs::write(work.join(".gitignore"), rule).unwrap();
			std::fs::create_dir_all(work.join(folder)).unwrap();
			std::fs::write(work.join(folder).join("a"), b"tracked\n").unwrap();
			git(&["-C", &w, "add", ".gitignore"]);
			git(&["-C", &w, "add", "-f", &format!("{folder}/a")]);
			commit(&w, "folder");
			git(&["-C", &w, "switch", "-q", "-c", "other"]);
			git(&["-C", &w, "rm", "-q", "-r", folder]);
			std::fs::create_dir_all(work.join(folder).parent().unwrap()).unwrap();
			std::fs::write(work.join(folder), b"a file\n").unwrap();
			git(&["-C", &w, "add", "-f", folder]);
			commit(&w, "file");
			git(&["-C", &w, "switch", "-q", "main"]);
			if let Some(junk) = junk {
				std::fs::write(work.join(junk), b"mine\n").unwrap();
			}
			let refused = if tool == "git" {
				!Command::new("git")
					.args(["-C", &w, "switch", "-q", "--no-overwrite-ignore", "other"])
					.output()
					.unwrap()
					.status
					.success()
			} else {
				let other = BranchName::parse("other").unwrap();
				match make_repo(&work)
					.switch_branch(&other, &switch_options(false), &TestIdentity)
					.await
				{
					Ok(()) => false,
					Err(SwitchError::Worktree(WorktreeError::IgnoredOverwrite(path))) => {
						assert_eq!(Some(path.to_string().as_str()), junk, "{rule}");
						true
					}
					Err(other) => panic!("{rule} {junk:?}: {other:?}"),
				}
			};
			outcomes.push((
				refused,
				git(&["-C", &w, "branch", "--show-current"]),
				junk.map(|junk| std::fs::read(work.join(junk)).unwrap()),
			));
		}
		assert_eq!(outcomes[1], outcomes[0], "{rule} {junk:?}");
		assert_eq!(outcomes[0].0, junk.is_some(), "{rule} {junk:?}");
	}
}

/// Told not to overwrite ignored files, a switch back to a branch with a submodule keeps an ignored
/// file inside the submodule's folder, as git's `switch --no-overwrite-ignore` does: the checkout
/// reuses the folder where the target puts a submodule, and what's in it stays.
#[tokio::test]
async fn switch_keeps_ignored_files_in_a_kept_submodule_folder_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-submodule-{tool}")) else {
			return;
		};
		let Some((_, source)) = repository(&format!("switch-submodule-source-{tool}")) else {
			return;
		};
		std::fs::write(work.join(".gitignore"), b"junk\n").unwrap();
		git(&["-C", &w, "add", ".gitignore"]);
		commit(&w, "ignore");
		git(&["-C", &w, "branch", "other"]);
		git(&[
			"-C",
			&w,
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			"-q",
			&source,
			"vendor",
		]);
		commit(&w, "submodule");
		// Away to a branch without it: git leaves the submodule's folder where it is.
		let _ = Command::new("git")
			.args(["-C", &w, "switch", "-q", "other"])
			.output()
			.unwrap();
		assert!(work.join("vendor/.git").exists());
		std::fs::write(work.join("vendor/junk"), b"mine\n").unwrap();
		if tool == "git" {
			git(&["-C", &w, "switch", "-q", "--no-overwrite-ignore", "main"]);
		} else {
			let main = BranchName::parse("main").unwrap();
			make_repo(&work)
				.switch_branch(&main, &switch_options(false), &TestIdentity)
				.await
				.unwrap();
		}
		outcomes.push((
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join("vendor/junk")).unwrap(),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(outcomes[1], ("main\n".to_owned(), "mine\n".to_owned()));
}

/// Told not to overwrite ignored files, a switch with no index file refuses to replace an ignored
/// file's local changes even where the two branches agree, as git's `switch --no-overwrite-ignore`
/// does: with no index, the checkout writes every path the target has.
#[tokio::test]
async fn switch_with_no_index_keeps_an_ignored_file_s_changes_as_git_does() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let work = unique_tmp(&format!("switch-no-index-{tool}"));
		let w = work.to_str().unwrap().to_owned();
		git(&["init", "--object-format=sha256", "-q", "-b", "main", &w]);
		// The only tracked file is ignored, so nothing else stops the checkout first.
		std::fs::write(work.join(".git/info/exclude"), b".env\n").unwrap();
		std::fs::write(work.join(".env"), b"committed\n").unwrap();
		git(&["-C", &w, "add", "-f", ".env"]);
		commit(&w, "env");
		git(&["-C", &w, "branch", "other"]);
		std::fs::write(work.join(".env"), b"local edit\n").unwrap();
		std::fs::remove_file(work.join(".git/index")).unwrap();
		let refused = if tool == "git" {
			!Command::new("git")
				.args(["-C", &w, "switch", "-q", "--no-overwrite-ignore", "other"])
				.output()
				.unwrap()
				.status
				.success()
		} else {
			let other = BranchName::parse("other").unwrap();
			match make_repo(&work)
				.switch_branch(&other, &switch_options(false), &TestIdentity)
				.await
			{
				Ok(()) => false,
				Err(SwitchError::Worktree(WorktreeError::IgnoredOverwrite(path))) => {
					assert_eq!(path.to_string(), ".env");
					true
				}
				Err(other) => panic!("{other:?}"),
			}
		};
		outcomes.push((
			refused,
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join(".env")).unwrap(),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(
		outcomes[1],
		(true, "main\n".to_owned(), "local edit\n".to_owned())
	);
}

/// A switch that would replace a folder holding a nested repository (its `.git` only) keeps it, as
/// git does: in a folder that isn't ignored always, and in an ignored one when told not to overwrite
/// ignored files (git's `--no-overwrite-ignore`), each refusal naming the `.git`. Told it may
/// overwrite ignored files, a switch replaces an ignored folder, nested repository and all, as git's
/// does.
#[tokio::test]
async fn switch_keeps_a_nested_repository_in_a_folder_it_would_replace_as_git_does() {
	// (ignore rule, may overwrite ignored files, how it's refused)
	let cases = [
		("folder/\n", false, Some("ignored")),
		("", false, Some("untracked")),
		("", true, Some("untracked")),
		("folder/\n", true, None),
	];
	for (rule, overwrite, refused_as) in cases {
		let mut outcomes = Vec::new();
		for tool in ["git", "gitana"] {
			let Some((work, w)) = repository(&format!("switch-nested-{tool}")) else {
				return;
			};
			std::fs::write(work.join(".gitignore"), rule).unwrap();
			git(&["-C", &w, "add", ".gitignore"]);
			commit(&w, "ignore");
			git(&["-C", &w, "switch", "-q", "-c", "other"]);
			std::fs::write(work.join("folder"), b"a file\n").unwrap();
			git(&["-C", &w, "add", "-f", "folder"]);
			commit(&w, "file");
			git(&["-C", &w, "switch", "-q", "main"]);
			git(&["init", "-q", work.join("folder").to_str().unwrap()]);
			let refused = if tool == "git" {
				let mut args = vec!["-C", w.as_str(), "switch", "-q"];
				if !overwrite {
					args.push("--no-overwrite-ignore");
				}
				args.push("other");
				!Command::new("git")
					.args(&args)
					.output()
					.unwrap()
					.status
					.success()
			} else {
				let other = BranchName::parse("other").unwrap();
				match make_repo(&work)
					.switch_branch(&other, &switch_options(overwrite), &TestIdentity)
					.await
				{
					Ok(()) => false,
					Err(SwitchError::Worktree(WorktreeError::IgnoredOverwrite(path))) => {
						assert_eq!(
							(refused_as, path.to_string().as_str()),
							(Some("ignored"), "folder/.git")
						);
						true
					}
					Err(SwitchError::Worktree(WorktreeError::UntrackedOverwrite(path))) => {
						assert_eq!(
							(refused_as, path.to_string().as_str()),
							(Some("untracked"), "folder/.git")
						);
						true
					}
					Err(other) => panic!("{other:?}"),
				}
			};
			outcomes.push((refused, work.join("folder/.git").is_dir()));
		}
		assert_eq!(outcomes[1], outcomes[0], "{rule:?} overwrite={overwrite}");
		assert_eq!(
			outcomes[1],
			(refused_as.is_some(), refused_as.is_some()),
			"{rule:?} overwrite={overwrite}"
		);
	}
}

/// Told not to overwrite ignored files, a switch leaves alone an ignored file standing where the
/// index already holds what the target puts below it (`folder/child`), as git's
/// `switch --no-overwrite-ignore` does: the checkout writes neither, so nothing is overwritten.
#[tokio::test]
async fn switch_keeps_an_ignored_file_over_a_path_the_index_already_holds_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-satisfied-{tool}")) else {
			return;
		};
		git(&["-C", &w, "switch", "-q", "-c", "other"]);
		std::fs::create_dir_all(work.join("folder")).unwrap();
		std::fs::write(work.join("folder/child"), b"child\n").unwrap();
		commit_all(&w, "child");
		git(&["-C", &w, "switch", "-q", "main"]);
		// The index takes other's `folder/child`; the working tree then has an ignored file
		// `folder` in its place.
		std::fs::create_dir_all(work.join("folder")).unwrap();
		std::fs::write(work.join("folder/child"), b"child\n").unwrap();
		git(&["-C", &w, "add", "folder/child"]);
		std::fs::remove_dir_all(work.join("folder")).unwrap();
		std::fs::write(work.join("folder"), b"mine\n").unwrap();
		std::fs::write(work.join(".git/info/exclude"), b"folder\n").unwrap();
		if tool == "git" {
			git(&["-C", &w, "switch", "-q", "--no-overwrite-ignore", "other"]);
		} else {
			let other = BranchName::parse("other").unwrap();
			make_repo(&work)
				.switch_branch(&other, &switch_options(false), &TestIdentity)
				.await
				.unwrap();
		}
		outcomes.push((
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join("folder")).unwrap(),
			git(&["-C", &w, "status", "--porcelain"]),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(
		outcomes[1],
		(
			"other\n".to_owned(),
			"mine\n".to_owned(),
			" D folder/child\n".to_owned()
		)
	);
}

/// Told not to overwrite ignored files, a switch over a sparse checkout that keeps a staged out-of-cone
/// `p/child` doesn't write the target's file `p` (git leaves it pending rather than clobber the staged
/// entry), so an ignored file beside the child stays, and the switch goes ahead, as git's
/// `switch --no-overwrite-ignore` does.
#[tokio::test]
async fn switch_leaves_ignored_files_beside_a_preserved_sparse_entry_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-df-preserved-{tool}")) else {
			return;
		};
		std::fs::create_dir_all(work.join("in")).unwrap();
		std::fs::write(work.join("in/a"), b"i\n").unwrap();
		commit_all(&w, "in");
		git(&["-C", &w, "switch", "-q", "-c", "other"]);
		std::fs::write(work.join("p"), b"file p\n").unwrap();
		commit_all(&w, "p");
		git(&["-C", &w, "switch", "-q", "main"]);
		git(&["-C", &w, "sparse-checkout", "set", "--cone", "in"]);
		std::fs::create_dir_all(work.join("p")).unwrap();
		std::fs::write(work.join("p/child"), b"child\n").unwrap();
		git(&["-C", &w, "add", "--sparse", "p/child"]);
		std::fs::write(work.join(".git/info/exclude"), b"*.secret\n").unwrap();
		std::fs::write(work.join("p/mine.secret"), b"secret\n").unwrap();
		if tool == "git" {
			git(&["-C", &w, "switch", "-q", "--no-overwrite-ignore", "other"]);
		} else {
			let other = BranchName::parse("other").unwrap();
			make_repo(&work)
				.switch_branch(&other, &switch_options(false), &TestIdentity)
				.await
				.unwrap();
		}
		outcomes.push((
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join("p/mine.secret")).unwrap(),
			git(&["-C", &w, "ls-files", "-s", "--", "p", "p/child"])
				.lines()
				.map(|line| line.split('\t').nth(1).unwrap_or_default().to_owned())
				.collect::<Vec<_>>(),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(outcomes[1].0, "other\n");
	assert_eq!(outcomes[1].1, "secret\n");
}

/// A switch that may overwrite ignored files (git's default) replaces a folder holding only tracked
/// files and an ignored one, even where the ignore rule is on a folder above it (`outer/` ignoring
/// `outer/folder/junk`), removing the ignored file, as git's switch does.
#[tokio::test]
async fn switch_overwrites_a_file_ignored_by_a_folder_above_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-ignored-above-{tool}")) else {
			return;
		};
		std::fs::write(work.join(".gitignore"), b"outer/\n").unwrap();
		std::fs::create_dir_all(work.join("outer/folder")).unwrap();
		std::fs::write(work.join("outer/folder/a"), b"tracked\n").unwrap();
		git(&["-C", &w, "add", ".gitignore"]);
		git(&["-C", &w, "add", "-f", "outer/folder/a"]);
		commit(&w, "folder");
		git(&["-C", &w, "switch", "-q", "-c", "other"]);
		git(&["-C", &w, "rm", "-q", "-r", "outer/folder"]);
		std::fs::create_dir_all(work.join("outer")).unwrap();
		std::fs::write(work.join("outer/folder"), b"a file\n").unwrap();
		git(&["-C", &w, "add", "-f", "outer/folder"]);
		commit(&w, "file");
		git(&["-C", &w, "switch", "-q", "main"]);
		std::fs::write(work.join("outer/folder/junk"), b"mine\n").unwrap();
		if tool == "git" {
			git(&["-C", &w, "switch", "-q", "other"]);
		} else {
			let other = BranchName::parse("other").unwrap();
			make_repo(&work)
				.switch_branch(&other, &switch_options(true), &TestIdentity)
				.await
				.unwrap();
		}
		outcomes.push((
			git(&["-C", &w, "branch", "--show-current"]),
			std::fs::read_to_string(work.join("outer/folder")).ok(),
		));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(
		outcomes[1],
		("other\n".to_owned(), Some("a file\n".to_owned()))
	);
}

/// A tracked file inside a folder that has since become a repository of its own isn't new: while a
/// merge has it in conflict, staging it resolves it, as `git add` does.
#[tokio::test]
async fn add_resolves_a_conflicted_tracked_file_inside_a_nested_repository() {
	let Some((work, w)) = repository("add-nested-conflict") else {
		return;
	};
	std::fs::create_dir_all(work.join("vendor")).unwrap();
	std::fs::write(work.join("vendor/file"), b"base\n").unwrap();
	commit_all(&w, "vendor");
	git(&["-C", &w, "switch", "-q", "-c", "other"]);
	std::fs::write(work.join("vendor/file"), b"theirs\n").unwrap();
	commit_all(&w, "theirs");
	git(&["-C", &w, "switch", "-q", "main"]);
	std::fs::write(work.join("vendor/file"), b"ours\n").unwrap();
	commit_all(&w, "ours");
	let _ = Command::new("git")
		.args([
			"-C",
			&w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
			"merge",
			"other",
		])
		.output()
		.unwrap();
	assert_eq!(
		git(&["-C", &w, "ls-files", "-u", "--", "vendor/file"])
			.lines()
			.count(),
		3
	);
	git(&[
		"init",
		"--object-format=sha256",
		"-q",
		work.join("vendor").to_str().unwrap(),
	]);
	std::fs::write(work.join("vendor/file"), b"resolved\n").unwrap();
	make_repo(&work)
		.add(&["vendor/file"], "", false, None)
		.await
		.unwrap();
	// One entry left, at stage 0, holding the resolution: what `git add vendor/file` leaves.
	let blob = git(&["-C", &w, "hash-object", "--", "vendor/file"]);
	assert_eq!(
		git(&["-C", &w, "ls-files", "-s", "--", "vendor/file"]).trim(),
		format!("100644 {} 0\tvendor/file", blob.trim())
	);
}

/// A branch checked out in a linked worktree whose admin folder's name isn't UTF-8 (git names it after
/// the worktree's folder) is still found, and switching to another branch isn't affected.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn switch_finds_a_branch_in_a_linked_worktree_whose_name_is_not_utf8() {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt as _;

	let Some((work, w)) = repository("switch-raw-name") else {
		return;
	};
	let linked = unique_tmp("switch-raw-name-linked").join(OsStr::from_bytes(b"wt-\xff"));
	let added = Command::new("git")
		.arg("-C")
		.arg(&work)
		.args(["worktree", "add", "-q", "-b", "taken"])
		.arg(&linked)
		.status()
		.unwrap();
	assert!(added.success());
	assert!(
		work
			.join(".git/worktrees")
			.join(OsStr::from_bytes(b"wt-\xff"))
			.is_dir()
	);
	git(&["-C", &w, "branch", "free"]);
	let wt = make_repo(&work);
	let taken = BranchName::parse("taken").unwrap();
	match wt
		.switch_branch(&taken, &switch_options(true), &TestIdentity)
		.await
	{
		Err(SwitchError::CheckedOutElsewhere { branch, .. }) => assert_eq!(branch, taken),
		other => panic!("not refused: {other:?}"),
	}
	let free = BranchName::parse("free").unwrap();
	wt.switch_branch(&free, &switch_options(true), &TestIdentity)
		.await
		.unwrap();
}

/// A stray file among the linked worktrees' admin folders (a `.DS_Store`, say) is passed over, as git
/// passes over it: switching still works, and a branch another worktree holds is still refused.
#[tokio::test]
async fn switch_passes_over_a_stray_file_among_the_worktrees_as_git_does() {
	let mut outcomes = Vec::new();
	for tool in ["git", "gitana"] {
		let Some((work, w)) = repository(&format!("switch-stray-{tool}")) else {
			return;
		};
		let linked = unique_tmp(&format!("switch-stray-linked-{tool}"));
		std::fs::remove_dir_all(&linked).unwrap();
		git(&[
			"-C",
			&w,
			"worktree",
			"add",
			"-q",
			"-b",
			"taken",
			linked.to_str().unwrap(),
		]);
		git(&["-C", &w, "branch", "free"]);
		std::fs::write(work.join(".git/worktrees/.DS_Store"), b"").unwrap();
		let (free, taken) = if tool == "git" {
			let switch = |branch: &str| {
				Command::new("git")
					.args(["-C", &w, "switch", "-q", branch])
					.output()
					.unwrap()
					.status
					.success()
			};
			(switch("free"), switch("taken"))
		} else {
			let wt = make_repo(&work);
			let free = BranchName::parse("free").unwrap();
			let taken = BranchName::parse("taken").unwrap();
			let free = wt
				.switch_branch(&free, &switch_options(true), &TestIdentity)
				.await
				.is_ok();
			let taken = match wt
				.switch_branch(&taken, &switch_options(true), &TestIdentity)
				.await
			{
				Ok(()) => true,
				Err(SwitchError::CheckedOutElsewhere { .. }) => false,
				Err(other) => panic!("{other:?}"),
			};
			(free, taken)
		};
		outcomes.push((free, taken));
	}
	assert_eq!(outcomes[1], outcomes[0]);
	assert_eq!(outcomes[1], (true, false));
}

#[tokio::test]
async fn add_refuses_a_nested_repository_but_not_a_registered_submodule() {
	let Some((work, w)) = repository("add-nested") else {
		return;
	};
	// `vendor` is a repository registered as a gitlink (stock git adds one for an embedded repository).
	let vendor = work.join("vendor");
	git(&[
		"init",
		"--object-format=sha256",
		"-q",
		vendor.to_str().unwrap(),
	]);
	commit_allow_empty(vendor.to_str().unwrap(), "v");
	git(&["-C", &w, "add", "vendor"]);
	commit(&w, "vendor");
	// `evil` is a repository of its own, not registered.
	let evil = work.join("evil");
	git(&[
		"init",
		"--object-format=sha256",
		"-q",
		evil.to_str().unwrap(),
	]);
	std::fs::write(evil.join("file"), b"x\n").unwrap();
	std::fs::write(work.join("b.txt"), b"B\n").unwrap();
	let wt = make_repo(&work);
	for pathspec in [".", "evil", "evil/file"] {
		match wt.add(&[pathspec], "", false, None).await {
			Err(WorktreeError::EmbeddedRepository(path)) => {
				assert_eq!(path.to_string(), "evil", "{pathspec}");
			}
			other => panic!("{pathspec}: not refused: {other:?}"),
		}
		assert_eq!(
			git(&["-C", &w, "diff", "--cached", "--name-only"]).trim(),
			"",
			"{pathspec} staged something"
		);
	}
	// What's beside it, and the registered one, stage.
	wt.add(&["b.txt", "vendor"], "", false, None).await.unwrap();
	assert_eq!(
		git(&["-C", &w, "diff", "--cached", "--name-only"]).trim(),
		"b.txt"
	);
}

fn commit(work: &str, msg: &str) {
	git(&[
		"-C",
		work,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"-c",
		"core.hooksPath=/dev/null",
		"commit",
		"-q",
		"-m",
		msg,
	]);
}

fn commit_all(work: &str, msg: &str) {
	git(&["-C", work, "add", "-A"]);
	commit(work, msg);
}

fn commit_allow_empty(work: &str, msg: &str) {
	git(&[
		"-C",
		work,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"--allow-empty",
		"-m",
		msg,
	]);
}

fn git(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

/// Runs git, expecting it to refuse; its error output.
fn git_refuses(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(!out.status.success(), "git {args:?} didn't refuse");
	String::from_utf8_lossy(&out.stderr).into_owned()
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!(
		"gitana-worktree-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-operations");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
