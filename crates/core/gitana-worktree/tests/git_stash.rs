//! `WorkTree::stash_push`, cross-checked against stock git: twin repositories made identically by
//! git, one stashed by git and one by gitana, compared by the stash commits' ids (equal ids mean
//! equal trees, messages, parents and identities), `logs/refs/stash`, and the working tree and
//! index left behind.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::Sha1;
use gitana_object_store::ObjectStore;
use gitana_path::{GitPath, GitPathspec};
use gitana_repository::Repository;
use gitana_worktree::{StashPush, StashPushError, WorkTree};

type Tree = WorkTree<LocalFileStore, CapWorkDir, Sha1>;

const WHEN: &str = "1700000000 +0000";

struct TestIdentity;

impl gitana_identity::Identity for TestIdentity {
	async fn author(&self) -> anyhow::Result<String> {
		Ok(format!("A <a@e> {WHEN}"))
	}
	async fn committer(&self) -> anyhow::Result<String> {
		Ok(format!("C <c@e> {WHEN}"))
	}
	async fn committer_or_default(&self) -> anyhow::Result<String> {
		self.committer().await
	}
}

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn worktree(work: &Path) -> Tree {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha1>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!(
		"gitana-wt-stash-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
	Command::new("git")
		.args(["-C", dir.to_str().unwrap()])
		.args(args)
		.env("HOME", dir.parent().unwrap())
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_AUTHOR_NAME", "A")
		.env("GIT_AUTHOR_EMAIL", "a@e")
		.env("GIT_COMMITTER_NAME", "C")
		.env("GIT_COMMITTER_EMAIL", "c@e")
		.env("GIT_AUTHOR_DATE", WHEN)
		.env("GIT_COMMITTER_DATE", WHEN)
		.output()
		.expect("run git")
}

fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_raw(dir, args);
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, path: &str, content: &[u8]) {
	let path = dir.join(path);
	std::fs::create_dir_all(path.parent().unwrap()).unwrap();
	std::fs::write(path, content).unwrap();
}

/// Two repositories `setup` makes alike (git `init`, then `setup`), for git and for gitana.
fn twins(tag: &str, setup: impl Fn(&Path)) -> (PathBuf, PathBuf, PathBuf) {
	let base = unique_tmp(tag);
	let (by_git, by_gitana) = (base.join("git"), base.join("gitana"));
	for dir in [&by_git, &by_gitana] {
		std::fs::create_dir_all(dir).unwrap();
		git(dir, &["init", "-q", "-b", "main"]);
		git(dir, &["config", "core.autocrlf", "false"]);
		setup(dir);
	}
	(base, by_git, by_gitana)
}

/// A first commit of `a.txt`, `b.txt`, `d/c.txt` and `.gitignore` (`*.log`).
fn committed(dir: &Path) {
	write(dir, "a.txt", b"one\n");
	write(dir, "b.txt", b"two\n");
	write(dir, "d/c.txt", b"three\n");
	write(dir, ".gitignore", b"*.log\n");
	git(dir, &["add", "."]);
	git(
		dir,
		&["commit", "-q", "-m", "first line\nsecond line\n\nbody"],
	);
}

fn request(pathspecs: &[&str], prefix: &str, message: Option<&str>, untracked: bool) -> StashPush {
	StashPush {
		pathspecs: pathspecs
			.iter()
			.map(|spec| GitPathspec::from_utf8(spec).unwrap())
			.collect(),
		prefix: GitPath::from_utf8(prefix).unwrap(),
		message: message.map(str::to_owned),
		include_untracked: untracked,
		excludes_file: None,
	}
}

/// Everything a stash leaves to compare: the stash commits, its reflog, `HEAD`'s reflog tail and
/// `ORIG_HEAD`, the index, the status and the working files.
fn state(dir: &Path) -> Vec<String> {
	let rev = |spec: &str| {
		String::from_utf8(git_raw(dir, &["rev-parse", "-q", "--verify", spec]).stdout).unwrap()
	};
	let read = |path: &str| {
		String::from_utf8_lossy(&std::fs::read(dir.join(".git").join(path)).unwrap_or_default())
			.into_owned()
	};
	let mut files = Vec::new();
	let mut stack = vec![dir.to_path_buf()];
	while let Some(folder) = stack.pop() {
		for entry in std::fs::read_dir(&folder).unwrap() {
			let entry = entry.unwrap();
			let path = entry.path();
			if path.file_name().unwrap() == ".git" {
				continue;
			}
			let rel = path.strip_prefix(dir).unwrap().display().to_string();
			if entry.file_type().unwrap().is_dir() {
				files.push(format!("{rel}/"));
				stack.push(path);
			} else {
				files.push(format!("{rel}={:?}", std::fs::read(&path).unwrap()));
			}
		}
	}
	files.sort();
	vec![
		rev("refs/stash"),
		rev("stash^1"),
		rev("stash^2"),
		rev("stash^3"),
		read("logs/refs/stash"),
		read("logs/HEAD")
			.lines()
			.last()
			.unwrap_or_default()
			.to_owned(),
		read("ORIG_HEAD"),
		git(dir, &["ls-files", "-s", "-t"]),
		git(dir, &["status", "--porcelain", "-uall", "--ignored"]),
		files.join("\n"),
	]
}

async fn push(dir: &Path, request: &StashPush) -> Result<Option<String>, StashPushError<Sha1>> {
	worktree(dir)
		.stash_push(request, &TestIdentity)
		.await
		.map(|pushed| pushed.map(|pushed| pushed.entry.message))
}

#[tokio::test]
async fn pushes_everything_as_git_does() {
	let (base, by_git, by_gitana) = twins("all", |dir| {
		committed(dir);
		write(dir, "a.txt", b"one!\n");
		write(dir, "b.txt", b"two staged\n");
		git(dir, &["add", "b.txt"]);
		write(dir, "b.txt", b"two staged then edited\n");
		std::fs::remove_file(dir.join("d/c.txt")).unwrap();
		write(dir, "new.txt", b"new\n");
		git(dir, &["add", "new.txt"]);
		write(dir, "u.txt", b"untracked\n");
	});
	git(&by_git, &["stash", "-q"]);
	let message = push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap();
	assert_eq!(
		message.as_deref(),
		Some(git(&by_git, &["stash", "list", "--format=%gs"]).trim())
	);
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// On a HEAD whose message is in another encoding (git's `encoding` header), the stash's "WIP on"
/// message quotes its subject in UTF-8, as git writes it: the stash commits are git's, byte for byte.
#[tokio::test]
async fn pushes_on_a_head_in_another_encoding_as_git_does() {
	let (base, by_git, by_gitana) = twins("latin1-head", |dir| {
		committed(dir);
		let message = dir.join(".git").join("latin1-message");
		std::fs::write(&message, b"Caf\xe9 cr\xe8me\n\nbr\xfbl\xe9e\n").unwrap();
		git(
			dir,
			&[
				"-c",
				"i18n.commitEncoding=ISO-8859-1",
				"commit",
				"-q",
				"--allow-empty",
				"-F",
				message.to_str().unwrap(),
			],
		);
		let raw = git_raw(dir, &["cat-file", "commit", "HEAD"]).stdout;
		assert!(raw.windows(20).any(|line| line == b"\nencoding ISO-8859-1"));
		write(dir, "a.txt", b"one!\n");
	});
	git(&by_git, &["stash", "-q"]);
	let message = push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap();
	assert_eq!(
		message.as_deref(),
		Some(git(&by_git, &["stash", "list", "--format=%gs"]).trim())
	);
	assert!(message.unwrap().ends_with("Caf\u{e9} cr\u{e8}me"));
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn pushes_with_a_message_and_on_a_detached_head_as_git_does() {
	for (message, detached) in [
		(Some("two\n  lines  "), false),
		(Some(""), false),
		(None, true),
	] {
		let (base, by_git, by_gitana) = twins("message", |dir| {
			committed(dir);
			git(dir, &["switch", "-q", "-c", "feat/x"]);
			if detached {
				git(dir, &["switch", "-q", "--detach"]);
			}
			write(dir, "a.txt", b"changed\n");
		});
		match message {
			Some(message) => git(&by_git, &["stash", "push", "-q", "-m", message]),
			None => git(&by_git, &["stash", "push", "-q"]),
		};
		push(&by_gitana, &request(&[], "", message, false))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "{message:?} {detached}");
		std::fs::remove_dir_all(&base).ok();
	}
}

#[tokio::test]
async fn pushes_only_the_paths_asked_as_git_does() {
	let (base, by_git, by_gitana) = twins("paths", |dir| {
		committed(dir);
		write(dir, "a.txt", b"one!\n");
		write(dir, "b.txt", b"two!\n");
		git(dir, &["add", "b.txt"]);
		write(dir, "d/c.txt", b"three!\n");
		write(dir, "d/new.txt", b"new\n");
		git(dir, &["add", "d/new.txt"]);
	});
	let out = git_raw(
		&by_git.join("d"),
		&["stash", "push", "-q", "--", "c.txt", "new.txt"],
	);
	assert!(
		out.status.success(),
		"{}",
		String::from_utf8_lossy(&out.stderr)
	);
	push(
		&by_gitana,
		&request(&["c.txt", "new.txt"], "d", None, false),
	)
	.await
	.unwrap()
	.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn pushes_untracked_files_as_git_does() {
	for pathspec in [None, Some("u")] {
		let (base, by_git, by_gitana) = twins("untracked", |dir| {
			committed(dir);
			write(dir, "a.txt", b"one!\n");
			write(dir, "u/v/new.txt", b"new\n");
			write(dir, "top.txt", b"top\n");
			write(dir, "skip.log", b"ignored\n");
			// Ignored by `.git/info/exclude` and by the global excludes file.
			write(dir, ".git/info/exclude", b"*.tmp\n");
			write(dir, "skip.tmp", b"ignored here\n");
			write(dir, "skip.bak", b"ignored globally\n");
			let global = dir.parent().unwrap().join("global-ignore");
			std::fs::write(&global, b"*.bak\n").unwrap();
			git(
				dir,
				&["config", "core.excludesFile", global.to_str().unwrap()],
			);
			std::fs::create_dir_all(dir.join("empty/dir")).unwrap();
		});
		let specs: Vec<&str> = pathspec.into_iter().collect();
		let mut args = vec!["stash", "push", "-q", "-u"];
		if !specs.is_empty() {
			args.push("--");
			args.extend(&specs);
		}
		git(&by_git, &args);
		let mut untracked = request(&specs, "", None, true);
		untracked.excludes_file = Some(b"*.bak\n".to_vec());
		push(&by_gitana, &untracked).await.unwrap().unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "{pathspec:?}");
		std::fs::remove_dir_all(&base).ok();
	}
}

#[tokio::test]
async fn puts_line_endings_away_as_git_does() {
	let (base, by_git, by_gitana) = twins("eol", |dir| {
		committed(dir);
		git(dir, &["config", "core.autocrlf", "true"]);
		write(dir, "a.txt", b"lf\nonly\n");
		write(dir, "b.txt", b"crlf\r\nhere\r\n");
	});
	// The index newer than the files, so the untouched files' stats aren't racy: git's stash refreshes
	// the index first and re-reads a racy file to tell it clean, where gitana's forced checkout rewrites
	// a racy file (logged, to raise separately).
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for dir in [&by_git, &by_gitana] {
		let index = std::fs::File::options()
			.append(true)
			.open(dir.join(".git/index"))
			.unwrap();
		index.set_modified(std::time::SystemTime::now()).unwrap();
	}
	let out = git_raw(&by_git, &["stash", "push", "-q"]);
	assert!(out.status.success());
	let pushed = worktree(&by_gitana)
		.stash_push(&request(&[], "", None, false), &TestIdentity)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	let warned: Vec<String> = pushed
		.line_ending_warnings
		.iter()
		.map(|(path, change)| format!("{path} {change:?}"))
		.collect();
	assert_eq!(
		warned.len(),
		String::from_utf8_lossy(&out.stderr)
			.matches("warning: in the working copy")
			.count()
	);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn refuses_as_git_does() {
	let (base, _, by_gitana) = twins("refuse", committed);
	// Nothing changed: nothing to save.
	assert!(
		push(&by_gitana, &request(&[], "", None, false))
			.await
			.unwrap()
			.is_none()
	);
	write(&by_gitana, "a.txt", b"one!\n");
	assert!(
		push(&by_gitana, &request(&["b.txt"], "", None, false))
			.await
			.unwrap()
			.is_none()
	);
	let error = push(&by_gitana, &request(&["nope.txt"], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(error, StashPushError::UnmatchedPathspec(_)),
		"{error:?}"
	);
	assert!(
		git_raw(&by_gitana, &["rev-parse", "-q", "--verify", "refs/stash"])
			.stdout
			.is_empty()
	);
	let fresh = base.join("fresh");
	std::fs::create_dir_all(&fresh).unwrap();
	git(&fresh, &["init", "-q", "-b", "main"]);
	write(&fresh, "f", b"x\n");
	git(&fresh, &["add", "f"]);
	let error = push(&fresh, &request(&[], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(error, StashPushError::NoInitialCommit),
		"{error:?}"
	);
	std::fs::remove_dir_all(&base).ok();
}

fn pop_request(index: usize, restore_index: bool) -> gitana_worktree::StashApply {
	gitana_worktree::StashApply {
		index,
		restore_index,
		excludes_file: None,
	}
}

fn stash_list(dir: &Path) -> String {
	git(dir, &["stash", "list"])
}

#[tokio::test]
async fn pops_as_git_does() {
	// (name, changes before the stash, a commit after it, restore_index)
	type Case = (&'static str, fn(&Path), Option<fn(&Path)>, bool);
	let cases: [Case; 5] = [
		(
			"same head",
			|dir| {
				write(dir, "a.txt", b"one!\n");
				write(dir, "b.txt", b"two staged\n");
				git(dir, &["add", "b.txt"]);
				write(dir, "n.txt", b"new\n");
				git(dir, &["add", "n.txt"]);
				std::fs::remove_file(dir.join("d/c.txt")).unwrap();
			},
			None,
			false,
		),
		(
			"moved head",
			|dir| write(dir, "a.txt", b"one!\n"),
			Some(|dir| {
				write(dir, "b.txt", b"two moved\n");
				git(dir, &["commit", "-q", "-am", "moved"]);
			}),
			false,
		),
		(
			"index restored",
			|dir| {
				write(dir, "a.txt", b"staged\n");
				git(dir, &["add", "a.txt"]);
				write(dir, "b.txt", b"unstaged\n");
			},
			None,
			true,
		),
		(
			"index skipped",
			|dir| write(dir, "a.txt", b"unstaged only\n"),
			None,
			true,
		),
		(
			"three-way content merge",
			|dir| write(dir, "a.txt", b"one\nmore\n"),
			Some(|dir| {
				write(dir, "a.txt", b"zero\none\n");
				git(dir, &["commit", "-q", "-am", "prepend"]);
			}),
			false,
		),
	];
	for (name, change, after, restore_index) in cases {
		let (base, by_git, by_gitana) = twins("pop", |dir| {
			committed(dir);
			change(dir);
			git(dir, &["stash", "-q"]);
			if let Some(after) = after {
				after(dir);
			}
		});
		let mut args = vec!["stash", "pop", "-q"];
		if restore_index {
			args.push("--index");
		}
		git(&by_git, &args);
		let applied = worktree(&by_gitana)
			.stash_pop(&pop_request(0, restore_index))
			.await
			.unwrap();
		assert!(applied.conflicts.is_empty(), "{name}");
		assert_eq!(state(&by_gitana), state(&by_git), "{name}");
		assert_eq!(stash_list(&by_gitana), stash_list(&by_git), "{name}");
		std::fs::remove_dir_all(&base).ok();
	}
}

#[tokio::test]
async fn a_conflicted_pop_keeps_the_stash_as_git_does() {
	let (base, by_git, by_gitana) = twins("conflict", |dir| {
		committed(dir);
		write(dir, "a.txt", b"stashed\n");
		write(dir, "b.txt", b"also stashed\n");
		git(dir, &["stash", "-q"]);
		write(dir, "a.txt", b"upstream\n");
		git(dir, &["commit", "-q", "-am", "up"]);
	});
	assert!(!git_raw(&by_git, &["stash", "pop", "-q"]).status.success());
	let applied = worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(
		applied.conflicts,
		vec![GitPath::from_utf8("a.txt").unwrap()]
	);
	assert_eq!(state(&by_gitana), state(&by_git));
	assert_eq!(stash_list(&by_gitana), stash_list(&by_git));
	assert!(
		std::fs::read_to_string(by_gitana.join("a.txt"))
			.unwrap()
			.contains("<<<<<<< Updated upstream")
	);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn brings_untracked_files_back_as_git_does() {
	let (base, by_git, by_gitana) = twins("pop-untracked", |dir| {
		committed(dir);
		write(dir, "a.txt", b"one!\n");
		write(dir, "u/v/new.txt", b"new\n");
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn refuses_to_pop_before_changing_anything() {
	use gitana_worktree::{StashApplyError, StashPopError};
	let (base, _, dir) = twins("pop-refuse", |dir| {
		committed(dir);
		write(dir, "a.txt", b"stashed\n");
		write(dir, "u.txt", b"untracked\n");
		git(dir, &["stash", "push", "-q", "-u"]);
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	let wt = worktree(&dir);
	let error = wt.stash_pop(&pop_request(3, false)).await.unwrap_err();
	assert!(
		matches!(
			error,
			StashPopError::Apply(StashApplyError::OutOfRange { index: 3, count: 1 })
		),
		"{error:?}"
	);
	// An untracked file the stash holds already exists: refused, nothing changed (git applies the
	// tracked part first).
	write(&dir, "u.txt", b"other\n");
	let before = state(&dir);
	let error = wt.stash_pop(&pop_request(0, false)).await.unwrap_err();
	assert!(
		matches!(&error, StashPopError::Apply(StashApplyError::UntrackedExists(path)) if path.as_bytes() == b"u.txt"),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	// A local change the stash would overwrite: refused, nothing changed.
	std::fs::remove_file(dir.join("u.txt")).unwrap();
	write(&dir, "a.txt", b"dirty\n");
	let before = state(&dir);
	let error = wt.stash_pop(&pop_request(0, false)).await.unwrap_err();
	assert!(
		matches!(error, StashPopError::Apply(StashApplyError::Worktree(_))),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	// `--index` whose staged changes don't apply: refused, nothing changed.
	git(&dir, &["checkout", "-q", "--", "a.txt"]);
	git(&dir, &["stash", "pop", "-q"]);
	write(&dir, "a.txt", b"staged\n");
	git(&dir, &["add", "a.txt"]);
	git(&dir, &["stash", "push", "-q"]);
	write(&dir, "a.txt", b"other staged\n");
	git(&dir, &["add", "a.txt"]);
	let before = state(&dir);
	let error = wt.stash_pop(&pop_request(0, true)).await.unwrap_err();
	assert!(
		matches!(error, StashPopError::Apply(StashApplyError::IndexConflicts)),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	// No stashes at all.
	git(&dir, &["stash", "clear"]);
	let error = wt.stash_pop(&pop_request(0, false)).await.unwrap_err();
	assert!(
		matches!(error, StashPopError::Apply(StashApplyError::NoEntries)),
		"{error:?}"
	);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn puts_modes_and_symlinks_away_and_back_as_git_does() {
	let (base, by_git, by_gitana) = twins("modes", |dir| {
		committed(dir);
		std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
		git(dir, &["add", "link"]);
		git(dir, &["commit", "-q", "-m", "link"]);
		let mut permissions = std::fs::metadata(dir.join("a.txt")).unwrap().permissions();
		std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
		std::fs::set_permissions(dir.join("a.txt"), permissions).unwrap();
		std::fs::remove_file(dir.join("link")).unwrap();
		std::os::unix::fs::symlink("b.txt", dir.join("link")).unwrap();
		std::os::unix::fs::symlink("d/c.txt", dir.join("new-link")).unwrap();
		git(dir, &["add", "new-link"]);
	});
	git(&by_git, &["stash", "-q"]);
	push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "push");
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "pop");
	std::fs::remove_dir_all(&base).ok();
}

/// Each tool takes the other's stashes: twins run the same sequence, one with git alone and one
/// alternating gitana and git, and end alike step by step.
#[tokio::test]
async fn takes_git_s_stashes_and_git_takes_gitana_s() {
	let (base, by_git, mixed) = twins("interop", committed);
	// gitana pushes, git pops it (`--index`).
	for dir in [&by_git, &mixed] {
		write(dir, "a.txt", b"staged\n");
		git(dir, &["add", "a.txt"]);
		write(dir, "b.txt", b"unstaged\n");
	}
	git(&by_git, &["stash", "push", "-q", "-m", "first"]);
	push(&mixed, &request(&[], "", Some("first"), false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&mixed), state(&by_git), "gitana's push");
	for dir in [&by_git, &mixed] {
		git(dir, &["stash", "pop", "-q", "--index"]);
	}
	assert_eq!(state(&mixed), state(&by_git), "git pops gitana's");
	// git pushes twice and gitana once, interleaved; git lists them alike.
	for dir in [&by_git, &mixed] {
		git(dir, &["stash", "push", "-q", "-m", "second"]);
		write(dir, "a.txt", b"third\n");
	}
	git(&by_git, &["stash", "push", "-q", "-m", "third"]);
	push(&mixed, &request(&[], "", Some("third"), false))
		.await
		.unwrap()
		.unwrap();
	for dir in [&by_git, &mixed] {
		write(dir, "d/c.txt", b"fourth\n");
		git(dir, &["stash", "push", "-q", "-m", "fourth"]);
	}
	assert_eq!(stash_list(&mixed), stash_list(&by_git), "interleaved");
	assert_eq!(state(&mixed), state(&by_git), "interleaved");
	// gitana drops the middle one and git the oldest; gitana pops git's.
	git(&by_git, &["stash", "drop", "-q", "stash@{1}"]);
	worktree(&mixed)
		.repository()
		.drop_stash(1, None)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&mixed), state(&by_git), "gitana's drop");
	for dir in [&by_git, &mixed] {
		git(dir, &["stash", "drop", "-q", "stash@{1}"]);
	}
	assert_eq!(state(&mixed), state(&by_git), "git's drop after gitana's");
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&mixed)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&mixed), state(&by_git), "gitana pops git's");
	assert_eq!(stash_list(&mixed), "");
	std::fs::remove_dir_all(&base).ok();
}

/// Converted line endings come back as git writes them: a tracked file changed with CRLF and an
/// untracked CRLF file, under `core.autocrlf=true`.
#[tokio::test]
async fn brings_line_endings_back_as_git_does() {
	let (base, by_git, by_gitana) = twins("eol-pop", |dir| {
		committed(dir);
		git(dir, &["config", "core.autocrlf", "true"]);
		write(dir, "a.txt", b"one\r\ntwo\r\n");
		write(dir, "u.txt", b"untracked\r\n");
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// A sparse checkout's skip-worktree entries keep their index content in the stash and stay
/// skipped through the reset and back, whole or with a pathspec inside the sparse set (git fails
/// half-way for a pathspec matching a skipped path; logged, to raise).
#[tokio::test]
async fn keeps_sparse_entries_as_git_does() {
	for pathspecs in [&[][..], &["e"][..]] {
		let (base, by_git, by_gitana) = twins("sparse", |dir| {
			committed(dir);
			write(dir, "e/f.txt", b"four\n");
			git(dir, &["add", "e/f.txt"]);
			git(dir, &["commit", "-q", "-m", "e"]);
			git(dir, &["sparse-checkout", "set", "--cone", "e"]);
			write(dir, "a.txt", b"one!\n");
			write(dir, "e/f.txt", b"four!\n");
		});
		let mut args = vec!["stash", "push", "-q"];
		if !pathspecs.is_empty() {
			args.push("--");
			args.extend(pathspecs);
		}
		git(&by_git, &args);
		push(&by_gitana, &request(pathspecs, "", None, false))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "push {pathspecs:?}");
		git(&by_git, &["stash", "pop", "-q"]);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "pop {pathspecs:?}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// An intent-to-add entry anywhere in the index refuses the push before anything changes, as git
/// does, naming every such entry.
#[tokio::test]
async fn refuses_intent_to_add_as_git_does() {
	use gitana_worktree::StashPushError;
	for (specs, untracked) in [(&[][..], false), (&["a.txt"][..], false), (&[][..], true)] {
		let (base, by_git, by_gitana) = twins("ita", |dir| {
			committed(dir);
			write(dir, "a.txt", b"one!\n");
			write(dir, "z.txt", b"z\n");
			write(dir, "m.txt", b"m\n");
			git(dir, &["add", "-N", "z.txt", "m.txt"]);
		});
		let mut args = vec!["stash", "push", "-q"];
		if untracked {
			args.push("-u");
		}
		if !specs.is_empty() {
			args.push("--");
			args.extend(specs);
		}
		assert!(!git_raw(&by_git, &args).status.success(), "{args:?}");
		let error = push(&by_gitana, &request(specs, "", None, untracked))
			.await
			.unwrap_err();
		assert!(
			matches!(&error, StashPushError::IntentToAdd(paths)
				if paths.iter().map(|path| path.as_bytes()).eq([&b"m.txt"[..], b"z.txt"])),
			"{args:?}: {error:?}"
		);
		assert_eq!(state(&by_gitana), state(&by_git), "{args:?}");
		assert!(!by_gitana.join(".git/index.lock").exists());
		std::fs::remove_dir_all(&base).ok();
	}
}

/// With untracked files, nested repositories are reported and left where they are, and a push that
/// finds only them still saves a stash with an empty untracked part, as git's does.
#[tokio::test]
async fn leaves_nested_repositories_out_as_git_does() {
	type Setup = fn(&Path);
	let cases: [(&str, Setup, &[&str]); 3] = [
		(
			"beside other changes",
			|dir| {
				write(dir, "a.txt", b"one!\n");
				write(dir, "top.txt", b"top\n");
				git(dir, &["init", "-q", "nested"]);
				write(dir, "nested/f", b"x\n");
			},
			&["nested"],
		),
		(
			"alone",
			|dir| {
				git(dir, &["init", "-q", "nested"]);
				write(dir, "nested/f", b"x\n");
			},
			&["nested"],
		),
		(
			"deeper",
			|dir| {
				git(dir, &["init", "-q", "deep/inner"]);
				write(dir, "deep/inner/g", b"q\n");
				write(dir, "deep/w.txt", b"w\n");
				git(dir, &["init", "-q", "nested"]);
			},
			&["deep/inner", "nested"],
		),
	];
	for (name, setup, skipped) in cases {
		let (base, by_git, by_gitana) = twins("nested", |dir| {
			committed(dir);
			setup(dir);
		});
		git(&by_git, &["stash", "push", "-q", "-u"]);
		let pushed = worktree(&by_gitana)
			.stash_push(&request(&[], "", None, true), &TestIdentity)
			.await
			.unwrap()
			.unwrap();
		assert!(
			pushed
				.skipped_repositories
				.iter()
				.map(|path| path.as_bytes())
				.eq(skipped.iter().map(|path| path.as_bytes())),
			"{name}: {:?}",
			pushed.skipped_repositories
		);
		assert_eq!(state(&by_gitana), state(&by_git), "{name}: push");
		git(&by_git, &["stash", "pop", "-q"]);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "{name}: pop");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A pathspec reaching only skip-worktree entries: the stash is git's, the paths inside the sparse
/// checkout are put back, and those outside are left as they are and reported (git saves the same
/// stash, then fails half-way through its clean-up).
#[tokio::test]
async fn reports_pathspecs_outside_the_sparse_checkout() {
	for staged_outside in [false, true] {
		let (base, by_git, by_gitana) = twins("sparse-advice", |dir| {
			committed(dir);
			write(dir, "e/f.txt", b"four\n");
			git(dir, &["add", "e/f.txt"]);
			git(dir, &["commit", "-q", "-m", "e"]);
			git(dir, &["sparse-checkout", "set", "--cone", "e"]);
			write(dir, "a.txt", b"one!\n");
			write(dir, "e/f.txt", b"four!\n");
			if staged_outside {
				let blob = git(dir, &["hash-object", "-w", "a.txt"]);
				let info = format!("100644,{},d/c.txt", blob.trim());
				git(dir, &["update-index", "--cacheinfo", &info]);
			}
		});
		assert!(
			!git_raw(&by_git, &["stash", "push", "-q", "--", "d", "e"])
				.status
				.success()
		);
		let index_before = git(&by_gitana, &["ls-files", "-s", "-t", "d"]);
		let pushed = worktree(&by_gitana)
			.stash_push(&request(&["d", "e"], "", None, false), &TestIdentity)
			.await
			.unwrap()
			.unwrap();
		assert!(
			pushed
				.sparse_pathspecs
				.iter()
				.map(|spec| spec.as_bytes())
				.eq([&b"d"[..]])
		);
		// The same stash as git's.
		assert_eq!(
			state(&by_gitana)[..5],
			state(&by_git)[..5],
			"{staged_outside}"
		);
		// Inside the sparse checkout, put back; outside, as it was.
		assert_eq!(std::fs::read(by_gitana.join("e/f.txt")).unwrap(), b"four\n");
		assert_eq!(std::fs::read(by_gitana.join("a.txt")).unwrap(), b"one!\n");
		assert!(!by_gitana.join("d").exists());
		assert_eq!(
			git(&by_gitana, &["ls-files", "-s", "-t", "d"]),
			index_before
		);
		assert_eq!(
			git(&by_gitana, &["status", "--porcelain", "--", "e"]),
			"",
			"{staged_outside}"
		);
		std::fs::remove_dir_all(&base).ok();
	}
}

/// What the clean-up would refuse is refused before the stash is stored: nothing saved, nothing
/// changed, the lock released. Here the reset would write back a deleted file whose attributes name
/// a configured filter, which gitana won't run.
#[tokio::test]
async fn refuses_what_the_clean_up_would_before_storing() {
	let (base, _, dir) = twins("cleanup-refusal", |dir| {
		committed(dir);
		write(dir, ".gitattributes", b"f.dat filter=up\n");
		git(dir, &["config", "filter.up.clean", "cat"]);
		git(dir, &["config", "filter.up.smudge", "cat"]);
		write(dir, "f.dat", b"data\n");
		git(dir, &["add", ".gitattributes", "f.dat"]);
		git(dir, &["commit", "-q", "-m", "filtered"]);
		std::fs::remove_file(dir.join("f.dat")).unwrap();
	});
	let before = state(&dir);
	let error = push(&dir, &request(&[], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			error,
			gitana_worktree::StashPushError::Worktree(
				gitana_worktree::WorktreeError::UnsupportedAttribute { .. }
			)
		),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	assert!(!dir.join(".git/index.lock").exists());
	std::fs::remove_dir_all(&base).ok();
}

/// A change the stash holds to a path the sparse checkout has since left out comes back, written to
/// the working tree with its skip-worktree bit cleared, as git's merge writes it. With
/// `restore_index`, git keeps the bit and the staged version but drops the stash's working version of
/// such a path; gitana keeps that too, written back (as you chose), and otherwise ends as git does.
#[tokio::test]
async fn brings_changes_outside_the_sparse_checkout_back() {
	// (name, changes before the stash, restore_index, whether git loses the working version)
	type Case = (&'static str, fn(&Path), bool, bool);
	let cases: [Case; 6] = [
		(
			"working",
			|dir| write(dir, "d/c.txt", b"working\n"),
			false,
			false,
		),
		(
			"staged",
			|dir| {
				write(dir, "d/c.txt", b"staged\n");
				git(dir, &["add", "d/c.txt"]);
			},
			false,
			false,
		),
		(
			"staged and working",
			|dir| {
				write(dir, "d/c.txt", b"staged\n");
				git(dir, &["add", "d/c.txt"]);
				write(dir, "d/c.txt", b"working\n");
			},
			false,
			false,
		),
		(
			"staged, with the index",
			|dir| {
				write(dir, "d/c.txt", b"staged\n");
				git(dir, &["add", "d/c.txt"]);
			},
			true,
			false,
		),
		(
			"staged and working, with the index",
			|dir| {
				write(dir, "d/c.txt", b"staged\n");
				git(dir, &["add", "d/c.txt"]);
				write(dir, "d/c.txt", b"working\n");
			},
			true,
			true,
		),
		(
			"working, with the index",
			|dir| {
				write(dir, "d/c.txt", b"working\n");
				write(dir, "a.txt", b"staged\n");
				git(dir, &["add", "a.txt"]);
			},
			true,
			true,
		),
	];
	for (name, change, restore_index, git_loses) in cases {
		let (base, by_git, by_gitana) = twins("sparse-back", |dir| {
			committed(dir);
			change(dir);
			git(dir, &["stash", "-q"]);
			git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		});
		let working = git(&by_git, &["show", "stash:d/c.txt"]);
		let mut args = vec!["stash", "pop", "-q"];
		if restore_index {
			args.push("--index");
		}
		git(&by_git, &args);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, restore_index))
			.await
			.unwrap();
		// Written back with the bit cleared, kept skipped only where the restored index holds the
		// stash's version (a staged change alone): the bit as stored (git clears it on reading a
		// file it finds present, unless told to expect such files).
		let raw = |dir: &Path| {
			git(
				dir,
				&[
					"-c",
					"sparse.expectFilesOutsideOfPatterns=true",
					"ls-files",
					"-t",
					"d",
				],
			)
		};
		let written_back = !restore_index || git_loses;
		let expected = if written_back {
			"H d/c.txt\n"
		} else {
			"S d/c.txt\n"
		};
		assert_eq!(raw(&by_gitana), expected, "{name}");
		if !git_loses {
			assert_eq!(raw(&by_gitana), raw(&by_git), "{name}");
		}
		if git_loses {
			// The index as git's, but the stash's working version written back, not lost.
			assert_eq!(
				git(&by_gitana, &["ls-files", "-s", "d"]),
				git(&by_git, &["ls-files", "-s", "d"]),
				"{name}"
			);
			assert_eq!(
				git(&by_gitana, &["ls-files", "-t", "d"]),
				"H d/c.txt\n",
				"{name}"
			);
			assert_eq!(
				std::fs::read_to_string(by_gitana.join("d/c.txt")).unwrap(),
				working,
				"{name}"
			);
			assert_eq!(stash_list(&by_gitana), "", "{name}");
		} else {
			assert_eq!(state(&by_gitana), state(&by_git), "{name}");
		}
		std::fs::remove_dir_all(&base).ok();
	}
	// A file at such a path with changes of its own is refused, as git refuses it, nothing changed.
	let (base, by_git, by_gitana) = twins("sparse-back-refused", |dir| {
		committed(dir);
		write(dir, "d/c.txt", b"working\n");
		git(dir, &["stash", "-q"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "d/c.txt", b"the user's\n");
	});
	assert!(!git_raw(&by_git, &["stash", "pop", "-q"]).status.success());
	let before = state(&by_gitana);
	let error = worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			&error,
			gitana_worktree::StashPopError::Apply(gitana_worktree::StashApplyError::Worktree(
				gitana_worktree::WorktreeError::Conflict(path)
			)) if path.as_bytes() == b"d/c.txt"
		),
		"{error:?}"
	);
	assert_eq!(state(&by_gitana), before);
	std::fs::remove_dir_all(&base).ok();
}

/// A stash that turned a file into a folder, and a folder into a file, pops to git's index: the new
/// entries staged and the ones they collide with gone, each reported.
#[tokio::test]
async fn replaces_files_and_folders_as_git_does() {
	let (base, by_git, by_gitana) = twins("replace", |dir| {
		committed(dir);
		git(dir, &["rm", "-q", "b.txt"]);
		write(dir, "b.txt/child", b"child\n");
		git(dir, &["rm", "-q", "-r", "d"]);
		write(dir, "d", b"a file now\n");
		git(dir, &["add", "b.txt/child", "d"]);
		git(dir, &["stash", "-q"]);
	});
	git(&by_git, &["stash", "pop", "-q"]);
	let applied = worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert!(
		applied
			.replaced
			.iter()
			.map(|path| path.as_bytes())
			.eq([&b"b.txt"[..], b"d/c.txt"])
	);
	assert_eq!(state(&by_gitana), state(&by_git));
	git(&by_gitana, &["write-tree"]);
	std::fs::remove_dir_all(&base).ok();
}

/// `<path>.stash.<6 random characters>` as `<path>.stash.XXXXXX`, so twins compare alike.
fn without_aside_suffix(lines: Vec<String>) -> Vec<String> {
	lines
		.into_iter()
		.map(|text| {
			let mut out = String::new();
			let mut rest = text.as_str();
			while let Some(at) = rest.find(".stash.") {
				let (head, tail) = rest.split_at(at + ".stash.".len());
				out.push_str(head);
				let suffix: String = tail.chars().take(6).collect();
				if suffix.len() == 6 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
					out.push_str("XXXXXX");
					rest = &tail[6..];
				} else {
					rest = tail;
				}
			}
			out.push_str(rest);
			out
		})
		.collect()
}

/// A file of the user's in the way of a new file the stash brings back outside the sparse
/// checkout is moved aside, as git moves it, and reported.
#[tokio::test]
async fn moves_a_file_in_the_way_aside_as_git_does() {
	let (base, by_git, by_gitana) = twins("aside", |dir| {
		committed(dir);
		write(dir, "d/new", b"stashed\n");
		git(dir, &["add", "d/new"]);
		git(dir, &["stash", "-q"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "d/new", b"the user's\n");
	});
	git(&by_git, &["stash", "pop", "-q"]);
	let applied = worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	let [(path, aside)] = applied.moved_aside.as_slice() else {
		panic!("{:?}", applied.moved_aside);
	};
	assert_eq!(path.as_bytes(), b"d/new");
	assert_eq!(
		std::fs::read(by_gitana.join(aside.to_string())).unwrap(),
		b"the user's\n"
	);
	assert_eq!(
		without_aside_suffix(state(&by_gitana)),
		without_aside_suffix(state(&by_git))
	);
	std::fs::remove_dir_all(&base).ok();
}

/// A deletion the stash holds of a path the sparse checkout has since left out comes back as a
/// deletion, as git's does, with or without the index.
#[tokio::test]
async fn brings_deletions_outside_the_sparse_checkout_back_as_git_does() {
	type Case = (&'static str, fn(&Path), bool);
	let cases: [Case; 5] = [
		(
			"staged",
			|dir| {
				git(dir, &["rm", "-q", "d/c.txt"]);
			},
			false,
		),
		(
			"working",
			|dir| std::fs::remove_file(dir.join("d/c.txt")).unwrap(),
			false,
		),
		(
			"staged, with the index",
			|dir| {
				git(dir, &["rm", "-q", "d/c.txt"]);
			},
			true,
		),
		(
			"working, with the index",
			|dir| {
				std::fs::remove_file(dir.join("d/c.txt")).unwrap();
				write(dir, "a.txt", b"staged\n");
				git(dir, &["add", "a.txt"]);
			},
			true,
		),
		(
			"changed, then deleted, with the index",
			|dir| {
				write(dir, "d/c.txt", b"staged\n");
				git(dir, &["add", "d/c.txt"]);
				std::fs::remove_file(dir.join("d/c.txt")).unwrap();
			},
			true,
		),
	];
	for (name, change, restore_index) in cases {
		let (base, by_git, by_gitana) = twins("sparse-deleted", |dir| {
			committed(dir);
			change(dir);
			git(dir, &["stash", "-q"]);
			git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		});
		let mut args = vec!["stash", "pop", "-q"];
		if restore_index {
			args.push("--index");
		}
		git(&by_git, &args);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, restore_index))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "{name}");
		let raw = |dir: &Path| {
			git(
				dir,
				&[
					"-c",
					"sparse.expectFilesOutsideOfPatterns=true",
					"ls-files",
					"-t",
					"d",
				],
			)
		};
		assert_eq!(raw(&by_gitana), raw(&by_git), "{name}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// An untracked file's blob the stash can't supply is found before anything changes.
#[tokio::test]
async fn refuses_a_stash_missing_an_untracked_blob_before_changing_anything() {
	let (base, _, dir) = twins("missing-blob", |dir| {
		committed(dir);
		write(dir, "a.txt", b"one!\n");
		write(dir, "u.txt", b"untracked\n");
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	let blob = git(&dir, &["rev-parse", "stash^3:u.txt"]);
	let blob = blob.trim();
	std::fs::remove_file(dir.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
	let before = state(&dir);
	let error = worktree(&dir)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			error,
			gitana_worktree::StashPopError::Apply(gitana_worktree::StashApplyError::Repository(_))
		),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	assert!(!dir.join(".git/index.lock").exists());
	std::fs::remove_dir_all(&base).ok();
}

/// A file removed from the index alone (`rm --cached`) and then edited is saved with its edit, as
/// git's working-tree commit covers `HEAD`'s paths, rather than lost to the reset.
#[tokio::test]
async fn saves_a_file_removed_from_the_index_only_as_git_does() {
	for (specs, untracked) in [(&[][..], false), (&[][..], true), (&["b.txt"][..], true)] {
		let (base, by_git, by_gitana) = twins("rm-cached", |dir| {
			committed(dir);
			git(dir, &["rm", "-q", "--cached", "b.txt"]);
			write(dir, "b.txt", b"edited\n");
			write(dir, "a.txt", b"one!\n");
		});
		let mut args = vec!["stash", "push", "-q"];
		if untracked {
			args.push("-u");
		}
		if !specs.is_empty() {
			args.push("--");
			args.extend(specs);
		}
		git(&by_git, &args);
		push(&by_gitana, &request(specs, "", None, untracked))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "push {args:?}");
		assert_eq!(
			git(&by_gitana, &["show", "stash:b.txt"]),
			"edited\n",
			"{args:?}"
		);
		if untracked {
			// The file is in the untracked part too, so the pop meets it already there: git
			// applies the tracked part and then fails, gitana refuses before changing anything.
			assert!(!git_raw(&by_git, &["stash", "pop", "-q"]).status.success());
			let before = state(&by_gitana);
			let error = worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap_err();
			assert!(
				matches!(
					&error,
					gitana_worktree::StashPopError::Apply(
						gitana_worktree::StashApplyError::UntrackedExists(path)
					) if path.as_bytes() == b"b.txt"
				),
				"{error:?}"
			);
			assert_eq!(state(&by_gitana), before, "{args:?}");
		} else {
			git(&by_git, &["stash", "pop", "-q"]);
			worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap();
			assert_eq!(state(&by_gitana), state(&by_git), "pop {args:?}");
		}
		std::fs::remove_dir_all(&base).ok();
	}
}

/// The skip-worktree bit the merge reaches is kept: a staged file outside the sparse checkout that
/// the merge removes as clean stays skipped and staged, as git leaves it.
#[tokio::test]
async fn keeps_the_skip_worktree_bit_the_merge_reaches_as_git_does() {
	let (base, by_git, by_gitana) = twins("sparse-reconciled", |dir| {
		committed(dir);
		write(dir, "a.txt", b"one!\n");
		git(dir, &["stash", "-q"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "d/c.txt", b"staged\n");
		git(dir, &["add", "--sparse", "d/c.txt"]);
	});
	git(&by_git, &["stash", "apply", "-q"]);
	worktree(&by_gitana)
		.stash_apply(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	let raw = |dir: &Path| {
		git(
			dir,
			&[
				"-c",
				"sparse.expectFilesOutsideOfPatterns=true",
				"ls-files",
				"-t",
				"d",
			],
		)
	};
	assert_eq!(raw(&by_gitana), raw(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// `git args`, with `input` on its standard input; its output, trimmed.
fn git_input(dir: &Path, args: &[&str], input: &[u8]) -> String {
	use std::io::Write;
	let mut child = Command::new("git")
		.args(["-C", dir.to_str().unwrap()])
		.args(args)
		.env("HOME", dir.parent().unwrap())
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_AUTHOR_NAME", "A")
		.env("GIT_AUTHOR_EMAIL", "a@e")
		.env("GIT_COMMITTER_NAME", "C")
		.env("GIT_COMMITTER_EMAIL", "c@e")
		.env("GIT_AUTHOR_DATE", WHEN)
		.env("GIT_COMMITTER_DATE", WHEN)
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.expect("run git");
	child.stdin.take().unwrap().write_all(input).unwrap();
	let out = child.wait_with_output().unwrap();
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A tree of `files` (`(path, blob)`), folders made as they're named.
fn mktree(dir: &Path, files: &[(String, String)]) -> String {
	let mut lines = Vec::new();
	let mut folders: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
	for (path, blob) in files {
		match path.split_once('/') {
			Some((folder, rest)) => folders
				.entry(folder.to_owned())
				.or_default()
				.push((rest.to_owned(), blob.clone())),
			None => lines.push(format!("100644 blob {blob}\t{path}")),
		}
	}
	for (folder, inner) in folders {
		lines.push(format!("040000 tree {}\t{folder}", mktree(dir, &inner)));
	}
	git_input(
		dir,
		&["mktree"],
		format!("{}\n", lines.join("\n")).as_bytes(),
	)
}

/// A stash built from objects, as a case-sensitive machine could make it: `HEAD`'s files with
/// `tracked` new ones staged, and `untracked` files, each `(path, content)`.
fn hand_made_stash(dir: &Path, tracked: &[(&str, &[u8])], untracked: &[(&str, &[u8])]) {
	let blob = |content: &[u8]| git_input(dir, &["hash-object", "-w", "--stdin"], content);
	let mut files: Vec<(String, String)> = git(dir, &["ls-tree", "-r", "HEAD"])
		.lines()
		.map(|line| {
			let (meta, path) = line.split_once('\t').unwrap();
			(path.to_owned(), meta.split(' ').nth(2).unwrap().to_owned())
		})
		.collect();
	files.extend(
		tracked
			.iter()
			.map(|(path, content)| (path.to_string(), blob(content))),
	);
	let tree = mktree(dir, &files);
	let head = git(dir, &["rev-parse", "HEAD"]).trim().to_owned();
	let i = git(
		dir,
		&["commit-tree", "-p", &head, "-m", "index on main", &tree],
	);
	let untracked: Vec<(String, String)> = untracked
		.iter()
		.map(|(path, content)| (path.to_string(), blob(content)))
		.collect();
	let u_tree = mktree(dir, &untracked);
	let u = git(
		dir,
		&["commit-tree", "-m", "untracked files on main", &u_tree],
	);
	let w = git(
		dir,
		&[
			"commit-tree",
			"-p",
			&head,
			"-p",
			i.trim(),
			"-p",
			u.trim(),
			"-m",
			"WIP on main",
			&tree,
		],
	);
	git(
		dir,
		&["update-ref", "--create-reflog", "refs/stash", w.trim()],
	);
}

/// Whether the filesystem at `dir` sees names differing only in case as one.
fn folds_case(dir: &Path) -> bool {
	let probe = dir.join("Case-Probe");
	std::fs::write(&probe, b"").unwrap();
	let folds = dir.join("case-probe").exists();
	std::fs::remove_file(&probe).unwrap();
	folds
}

/// Untracked files the filesystem would see as one name, or as one of the merge's files, are
/// refused before anything is written, the stash kept (git writes the first, then fails). Where the
/// filesystem tells them apart, they're all restored, as git restores them.
#[tokio::test]
async fn refuses_untracked_files_the_filesystem_sees_as_one() {
	type Files = &'static [(&'static str, &'static [u8])];
	let cases: [(&str, Files, Files, &str); 4] = [
		(
			"two untracked",
			&[],
			&[("Foo", b"upper\n"), ("foo", b"lower\n")],
			"foo",
		),
		(
			"a file and a folder",
			&[],
			&[("P", b"file\n"), ("p/x", b"in a folder\n")],
			"p/x",
		),
		(
			"tracked and untracked",
			&[("Foo", b"tracked\n")],
			&[("foo", b"untracked\n")],
			"foo",
		),
		(
			"a folder and a file",
			&[],
			&[("P/x", b"in a folder\n"), ("p", b"file\n")],
			"p",
		),
	];
	for (name, tracked, untracked, refused) in cases {
		let (base, by_git, by_gitana) = twins("fold", |dir| {
			committed(dir);
			hand_made_stash(dir, tracked, untracked);
		});
		let git_popped = git_raw(&by_git, &["stash", "pop", "-q"]).status.success();
		if folds_case(&base) {
			assert!(!git_popped, "{name}");
			let before = state(&by_gitana);
			let error = worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap_err();
			assert!(
				matches!(
					&error,
					gitana_worktree::StashPopError::Apply(
						gitana_worktree::StashApplyError::UntrackedExists(path)
					) if path.as_bytes() == refused.as_bytes()
				),
				"{name}: {error:?}"
			);
			assert_eq!(state(&by_gitana), before, "{name}");
		} else {
			assert!(git_popped, "{name}");
			worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap();
			assert_eq!(state(&by_gitana), state(&by_git), "{name}");
		}
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A tracked file replaced by a folder of untracked files: the stash pops back, the merge deleting
/// the file before the untracked files are written, as git's does.
#[tokio::test]
async fn pops_a_file_replaced_by_untracked_files_as_git_does() {
	let (base, by_git, by_gitana) = twins("file-to-untracked", |dir| {
		committed(dir);
		git(dir, &["rm", "-q", "b.txt"]);
		write(dir, "b.txt/u", b"untracked\n");
	});
	git(&by_git, &["stash", "push", "-q", "-u"]);
	push(&by_gitana, &request(&[], "", None, true))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "push");
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "pop");
	std::fs::remove_dir_all(&base).ok();
}

/// An untracked file in the clean-up's way that the stash doesn't hold is refused before anything
/// is stored, rather than lost (git's whole push deletes it, its path-limited one fails part-way):
/// a folder where a tracked file was, a file where a tracked folder was, and an ignored file `-u`
/// doesn't put away. With `-u` holding the folder's files, the push and pop go as git's.
#[tokio::test]
async fn refuses_to_lose_untracked_files_the_stash_doesnt_hold() {
	type Case = (
		&'static str,
		fn(&Path),
		&'static [&'static str],
		bool,
		&'static str,
	);
	let cases: [Case; 4] = [
		(
			"a folder where a file was",
			|dir| {
				std::fs::remove_file(dir.join("b.txt")).unwrap();
				write(dir, "b.txt/precious", b"precious\n");
			},
			&[],
			false,
			"b.txt/precious",
		),
		(
			"the same, path-limited",
			|dir| {
				std::fs::remove_file(dir.join("b.txt")).unwrap();
				write(dir, "b.txt/precious", b"precious\n");
			},
			&["b.txt", "a.txt"],
			false,
			"b.txt/precious",
		),
		(
			"a file where a folder was",
			|dir| {
				std::fs::remove_dir_all(dir.join("d")).unwrap();
				write(dir, "d", b"precious\n");
			},
			&[],
			false,
			"d",
		),
		(
			"an ignored file with -u",
			|dir| {
				std::fs::remove_file(dir.join("b.txt")).unwrap();
				write(dir, "b.txt/precious.log", b"precious\n");
			},
			&[],
			true,
			"b.txt/precious.log",
		),
	];
	for (name, change, specs, untracked, refused) in cases {
		let (base, _, dir) = twins("unsaved", |dir| {
			committed(dir);
			write(dir, "a.txt", b"one!\n");
			change(dir);
		});
		let before = state(&dir);
		let error = push(&dir, &request(specs, "", None, untracked))
			.await
			.unwrap_err();
		assert!(
			matches!(
				&error,
				StashPushError::Worktree(gitana_worktree::WorktreeError::UntrackedOverwrite(path))
					if path.as_bytes() == refused.as_bytes()
			),
			"{name}: {error:?}"
		);
		assert_eq!(state(&dir), before, "{name}");
		assert!(!dir.join(".git/index.lock").exists(), "{name}");
		std::fs::remove_dir_all(&base).ok();
	}
	let (base, by_git, by_gitana) = twins("unsaved-held", |dir| {
		committed(dir);
		std::fs::remove_file(dir.join("b.txt")).unwrap();
		write(dir, "b.txt/precious", b"precious\n");
	});
	git(&by_git, &["stash", "push", "-q", "-u"]);
	push(&by_gitana, &request(&[], "", None, true))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "held: push");
	std::fs::remove_dir_all(&base).ok();
}

/// A selected `.gitattributes` deleted from the working tree no longer governs what's put away: a
/// CRLF file it said to store as LF is saved as it is, and comes back so, as git's stash does.
#[tokio::test]
async fn forgets_a_deleted_gitattributes_as_git_does() {
	for specs in [&[][..], &["a.txt", ".gitattributes"][..]] {
		let (base, by_git, by_gitana) = twins("deleted-attributes", |dir| {
			committed(dir);
			write(dir, ".gitattributes", b"a.txt text eol=lf\n");
			git(dir, &["add", ".gitattributes"]);
			git(dir, &["commit", "-q", "-m", "attributes"]);
			std::fs::remove_file(dir.join(".gitattributes")).unwrap();
			write(dir, "a.txt", b"one\r\n");
		});
		let mut args = vec!["stash", "push", "-q"];
		if !specs.is_empty() {
			args.push("--");
			args.extend(specs);
		}
		git(&by_git, &args);
		push(&by_gitana, &request(specs, "", None, false))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "push {specs:?}");
		// The entry's stat older than the pop's index write, so it isn't racy (a racy one is re-read
		// whatever its stat says): a stale stat left on it would then claim the file is changed.
		for dir in [&by_git, &by_gitana] {
			let status = Command::new("touch")
				.args(["-t", "202001010000"])
				.arg(dir.join("a.txt"))
				.status()
				.unwrap();
			assert!(status.success());
			git(dir, &["update-index", "-q", "--refresh"]);
		}
		git(&by_git, &["stash", "pop", "-q"]);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "pop {specs:?}");
		assert_eq!(std::fs::read(by_gitana.join("a.txt")).unwrap(), b"one\r\n");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// Whether `dir/path` was last written in the same second as its index: a racily-clean file.
fn racy_file(dir: &Path, path: &str) -> bool {
	let second = |path: PathBuf| {
		std::fs::metadata(path)
			.unwrap()
			.modified()
			.unwrap()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_secs()
	};
	second(dir.join(path)) == second(dir.join(".git/index"))
}

/// A racily-clean, unchanged file is left alone by a forced reset and by a stash push's clean-up,
/// as git's reset leaves it: read, found unchanged, not rewritten, even with `core.autocrlf`
/// turned on since (which would rewrite LF as CRLF).
#[tokio::test]
async fn leaves_unchanged_racy_files_alone_as_git_does() {
	for stash in [false, true] {
		let mut made = None;
		for _ in 0..20 {
			let (base, by_git, by_gitana) = twins("racy", |dir| {
				write(dir, "f", b"one\ntwo\n");
				write(dir, "a.txt", b"a\n");
				git(dir, &["add", "."]);
				git(dir, &["commit", "-q", "-m", "base"]);
			});
			if racy_file(&by_git, "f") && racy_file(&by_gitana, "f") {
				made = Some((base, by_git, by_gitana));
				break;
			}
			std::fs::remove_dir_all(&base).ok();
		}
		let Some((base, by_git, by_gitana)) = made else {
			panic!("no racily-clean file in 20 tries");
		};
		for dir in [&by_git, &by_gitana] {
			write(dir, "a.txt", b"a!\n");
			git(dir, &["config", "core.autocrlf", "true"]);
		}
		if stash {
			git(&by_git, &["stash", "push", "-q"]);
			push(&by_gitana, &request(&[], "", None, false))
				.await
				.unwrap()
				.unwrap();
		} else {
			git(&by_git, &["reset", "-q", "--hard"]);
			worktree(&by_gitana)
				.reset(
					"HEAD",
					gitana_worktree::ResetMode::Hard,
					&TestIdentity,
					None,
				)
				.await
				.unwrap();
		}
		assert_eq!(
			std::fs::read(by_gitana.join("f")).unwrap(),
			b"one\ntwo\n",
			"stash {stash}"
		);
		assert_eq!(state(&by_gitana), state(&by_git), "stash {stash}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A path-limited push whose path differs from `HEAD` only in a form git's comparison normalises
/// away saves git's stash, and leaves the file's bytes as they are (git's clean-up, failing there,
/// leaves them too), its entry taking the file's stat as git's does.
#[tokio::test]
async fn leaves_a_path_the_stash_holds_no_change_for_alone() {
	let (base, by_git, by_gitana) = twins("no-change", |dir| {
		committed(dir);
		write(dir, ".gitattributes", b"a.txt text eol=lf\n");
		git(dir, &["add", ".gitattributes"]);
		git(dir, &["commit", "-q", "-m", "attributes"]);
		std::fs::remove_file(dir.join(".gitattributes")).unwrap();
		write(dir, "a.txt", b"one\r\n");
	});
	// git saves the stash, then its clean-up fails ("No valid patches in input").
	assert!(
		!git_raw(&by_git, &["stash", "push", "-q", "--", "a.txt"])
			.status
			.success()
	);
	push(&by_gitana, &request(&["a.txt"], "", None, false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(std::fs::read(by_gitana.join("a.txt")).unwrap(), b"one\r\n");
	assert_eq!(state(&by_gitana), state(&by_git), "push");
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "pop");
	std::fs::remove_dir_all(&base).ok();
}

/// A file staged as new and then deleted from the working tree keeps its staged copy in the
/// stash's working tree, as git's does, so a pop brings it back staged.
#[tokio::test]
async fn keeps_a_deleted_staged_addition_as_git_does() {
	for restore_index in [false, true] {
		let (base, by_git, by_gitana) = twins("added-deleted", |dir| {
			committed(dir);
			write(dir, "new", b"new\n");
			git(dir, &["add", "new"]);
			std::fs::remove_file(dir.join("new")).unwrap();
		});
		git(&by_git, &["stash", "push", "-q"]);
		push(&by_gitana, &request(&[], "", None, false))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "push");
		let mut args = vec!["stash", "pop", "-q"];
		if restore_index {
			args.push("--index");
		}
		git(&by_git, &args);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, restore_index))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git), "pop {restore_index}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A push during a resolved merge whose `merge --autostash` left an autostash: the merge is
/// abandoned by the push's reset and the autostash saved on top of the new stash, as git leaves it.
#[tokio::test]
async fn saves_a_merge_s_autostash_on_top_as_git_does() {
	let (base, by_git, by_gitana) = twins("autostash", |dir| {
		committed(dir);
		git(dir, &["switch", "-q", "-c", "side"]);
		write(dir, "d/c.txt", b"side\n");
		git(dir, &["commit", "-q", "-am", "side"]);
		git(dir, &["switch", "-q", "main"]);
		write(dir, "d/c.txt", b"main\n");
		git(dir, &["commit", "-q", "-am", "main"]);
		write(dir, "a.txt", b"dirty\n");
		assert!(
			!git_raw(dir, &["merge", "-q", "--autostash", "side"])
				.status
				.success()
		);
		assert!(dir.join(".git/MERGE_AUTOSTASH").exists());
		write(dir, "d/c.txt", b"resolved\n");
		git(dir, &["add", "d/c.txt"]);
	});
	git(&by_git, &["stash", "push", "-q"]);
	push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	assert_eq!(stash_list(&by_gitana), stash_list(&by_git));
	assert!(!by_gitana.join(".git/MERGE_HEAD").exists());
	assert!(!by_gitana.join(".git/MERGE_AUTOSTASH").exists());
	std::fs::remove_dir_all(&base).ok();
}

/// A nested checkout whose `.git` is a file (a linked worktree, or `--separate-git-dir`) is a
/// nested repository to `-u`, left out as git leaves it.
#[tokio::test]
async fn leaves_gitfile_checkouts_out_as_git_does() {
	for separate in [false, true] {
		let (base, by_git, by_gitana) = twins("gitfile", |dir| {
			committed(dir);
			if separate {
				let elsewhere = dir.parent().unwrap().join(format!(
					"separate-{}",
					dir.file_name().unwrap().to_string_lossy()
				));
				git(
					dir,
					&[
						"init",
						"-q",
						"--separate-git-dir",
						elsewhere.to_str().unwrap(),
						"nested",
					],
				);
			} else {
				git(dir, &["branch", "-q", "side"]);
				git(dir, &["worktree", "add", "-q", "nested", "side"]);
			}
			write(dir, "nested/inner.txt", b"inner\n");
			write(dir, "top.txt", b"top\n");
			write(dir, "a.txt", b"one!\n");
		});
		git(&by_git, &["stash", "push", "-q", "-u"]);
		let pushed = worktree(&by_gitana)
			.stash_push(&request(&[], "", None, true), &TestIdentity)
			.await
			.unwrap()
			.unwrap();
		assert!(
			pushed
				.skipped_repositories
				.iter()
				.map(|path| path.as_bytes())
				.eq([&b"nested"[..]]),
			"{separate}"
		);
		assert_eq!(state(&by_gitana), state(&by_git), "{separate}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A file outside the sparse checkout that's been recreated and edited is saved, as git saves it
/// (clearing the skip-worktree bit of a file it finds present), so a pop brings the edit back.
#[tokio::test]
async fn saves_a_recreated_sparse_file_as_git_does() {
	let (base, by_git, by_gitana) = twins("sparse-recreated", |dir| {
		committed(dir);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "d/c.txt", b"recreated\n");
		write(dir, "a.txt", b"one!\n");
	});
	git(&by_git, &["stash", "push", "-q"]);
	push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "push");
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git), "pop");
	assert_eq!(
		std::fs::read(by_gitana.join("d/c.txt")).unwrap(),
		b"recreated\n"
	);
	std::fs::remove_dir_all(&base).ok();
}

/// The stash's commits and reflog alone (no `git status`, which errors on some working trees).
fn stash_commits(dir: &Path) -> Vec<String> {
	let rev = |spec: &str| {
		String::from_utf8(git_raw(dir, &["rev-parse", "-q", "--verify", spec]).stdout).unwrap()
	};
	vec![
		rev("refs/stash"),
		rev("stash^1"),
		rev("stash^2"),
		rev("stash^3"),
		String::from_utf8_lossy(&std::fs::read(dir.join(".git/logs/refs/stash")).unwrap_or_default())
			.into_owned(),
	]
}

/// A file or symlink standing where a submodule was is saved, as git saves it, rather than lost to
/// the reset that puts the submodule's folder back; a pop brings it back.
#[tokio::test]
async fn saves_a_file_in_place_of_a_submodule_as_git_does() {
	for symlink in [false, true] {
		let (base, by_git, by_gitana) = twins("in-place-of-submodule", |dir| {
			committed(dir);
			let source = dir.parent().unwrap().join("submodule-source");
			if !source.exists() {
				std::fs::create_dir_all(&source).unwrap();
				git(&source, &["init", "-q", "-b", "main"]);
				write(&source, "s", b"s\n");
				git(&source, &["add", "."]);
				git(&source, &["commit", "-q", "-m", "s"]);
			}
			git(
				dir,
				&[
					"-c",
					"protocol.file.allow=always",
					"submodule",
					"add",
					"-q",
					source.to_str().unwrap(),
					"sub",
				],
			);
			git(dir, &["commit", "-q", "-m", "sub"]);
			std::fs::remove_dir_all(dir.join("sub")).unwrap();
			if symlink {
				std::os::unix::fs::symlink("a.txt", dir.join("sub")).unwrap();
			} else {
				write(dir, "sub", b"precious\n");
			}
			write(dir, "a.txt", b"one!\n");
		});
		git(&by_git, &["stash", "push", "-q"]);
		push(&by_gitana, &request(&[], "", None, false))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(
			stash_commits(&by_gitana),
			stash_commits(&by_git),
			"push {symlink}"
		);
		let git_popped = git_raw(&by_git, &["stash", "pop", "-q"]).status.success();
		let gitana_popped = worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.is_ok();
		assert_eq!(gitana_popped, git_popped, "pop {symlink}");
		if symlink {
			// git's own status errors on a symlink at a submodule path ("expected submodule path
			// 'sub' not to be a symbolic link"), so the index and the link are compared.
			assert_eq!(
				git(&by_gitana, &["ls-files", "-s"]),
				git(&by_git, &["ls-files", "-s"])
			);
			assert_eq!(
				std::fs::read_link(by_gitana.join("sub")).unwrap(),
				std::fs::read_link(by_git.join("sub")).unwrap()
			);
		} else {
			assert_eq!(state(&by_gitana), state(&by_git), "pop");
		}
		std::fs::remove_dir_all(&base).ok();
	}
}

/// `core.symlinks=false` and `core.fileMode=false` in what a stash saves, as git saves it: an edited
/// symlink placeholder stays a symlink, a file's executable bit stays as recorded.
#[tokio::test]
async fn keeps_recorded_modes_where_config_says_as_git_does() {
	use std::os::unix::fs::PermissionsExt;
	let (base, by_git, by_gitana) = twins("config-modes", |dir| {
		committed(dir);
		std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
		git(dir, &["add", "link"]);
		git(dir, &["commit", "-q", "-m", "link"]);
		git(dir, &["config", "core.symlinks", "false"]);
		git(dir, &["config", "core.fileMode", "false"]);
		std::fs::remove_file(dir.join("link")).unwrap();
		write(dir, "link", b"b.txt");
		write(dir, "a.txt", b"one!\n");
		std::fs::set_permissions(dir.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
	});
	git(&by_git, &["stash", "push", "-q"]);
	push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap()
		.unwrap();
	// The stash git's (what's compared here); the reset after it differs for `link`, which git
	// writes back as a placeholder file under `core.symlinks=false` and gitana as a symlink (its
	// checkout follows what the filesystem can do; logged, to raise).
	assert_eq!(state(&by_gitana)[..5], state(&by_git)[..5]);
	std::fs::remove_dir_all(&base).ok();
}

/// A staged version whose blob is missing from the object store is refused before anything is
/// written, as git's `write-tree` refuses it ("invalid object"), rather than saved unreadable and
/// its file then removed.
#[tokio::test]
async fn refuses_a_missing_staged_blob_before_changing_anything() {
	let (base, _, dir) = twins("missing-staged-blob", |dir| {
		committed(dir);
		write(dir, "precious", b"precious\n");
		git(dir, &["add", "precious"]);
	});
	let blob = git(&dir, &["rev-parse", ":precious"]);
	let blob = blob.trim();
	std::fs::remove_file(dir.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
	let before = state(&dir);
	let error = push(&dir, &request(&[], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			error,
			StashPushError::Repository(gitana_repository::RepositoryError::MissingObject(_))
		),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	assert_eq!(std::fs::read(dir.join("precious")).unwrap(), b"precious\n");
	std::fs::remove_dir_all(&base).ok();
}

/// Untracked names the filesystem itself treats as one beyond ASCII case (`é` as one character and
/// as `e` with a combining accent, `É` and `é` on macOS): the filesystem is the judge, so a pop
/// refuses with nothing left behind and the stash kept; where the filesystem tells them apart,
/// both are restored, as git restores them.
#[tokio::test]
async fn refuses_untracked_names_the_filesystem_sees_as_one_beyond_ascii() {
	let pairs: [(&str, &str); 2] = [("\u{e9}", "e\u{301}"), ("\u{c9}", "\u{e9}")];
	for (first, second) in pairs {
		let (base, by_git, by_gitana) = twins("unicode-names", |dir| {
			committed(dir);
			hand_made_stash(dir, &[], &[(first, b"first\n"), (second, b"second\n")]);
		});
		let probe = base.join(first);
		std::fs::write(&probe, b"").unwrap();
		let one_name = base.join(second).exists();
		std::fs::remove_file(&probe).unwrap();
		let git_popped = git_raw(&by_git, &["stash", "pop", "-q"]).status.success();
		if one_name {
			assert!(!git_popped);
			let before = state(&by_gitana);
			let error = worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap_err();
			assert!(
				matches!(
					error,
					gitana_worktree::StashPopError::Apply(gitana_worktree::StashApplyError::UntrackedExists(
						_
					))
				),
				"{error:?}"
			);
			assert_eq!(state(&by_gitana), before, "{first:?}/{second:?}");
		} else {
			assert!(git_popped);
			worktree(&by_gitana)
				.stash_pop(&pop_request(0, false))
				.await
				.unwrap();
			assert_eq!(state(&by_gitana), state(&by_git), "{first:?}/{second:?}");
		}
		std::fs::remove_dir_all(&base).ok();
	}
}

/// An included path whose skip-worktree bit was set by hand, replaced by a folder of untracked
/// files the stash doesn't hold: the whole-tree reset would rewrite it (it follows the sparse
/// patterns), so the push refuses before anything changes, as review 213 settled.
#[tokio::test]
async fn refuses_to_lose_untracked_files_at_a_hand_skipped_path() {
	let (base, _, dir) = twins("hand-skipped", |dir| {
		committed(dir);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		// Kept: git otherwise clears the bit on any path it finds on disk as it reads the index
		// (the comparisons below would), where gitana's push sees it set.
		git(
			dir,
			&["config", "sparse.expectFilesOutsideOfPatterns", "true"],
		);
		git(dir, &["update-index", "--skip-worktree", "b.txt"]);
		std::fs::remove_file(dir.join("b.txt")).unwrap();
		write(dir, "b.txt/precious", b"precious\n");
		write(dir, "a.txt", b"one!\n");
	});
	let before = state(&dir);
	let error = push(&dir, &request(&[], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			&error,
			StashPushError::Worktree(gitana_worktree::WorktreeError::UntrackedOverwrite(path))
				if path.as_bytes() == b"b.txt/precious"
		),
		"{error:?}"
	);
	assert_eq!(state(&dir), before);
	std::fs::remove_dir_all(&base).ok();
}

/// A staged version whose stored copy is corrupt (present, but not what its id says) is refused
/// before anything changes, where git saves it and loses the file.
#[tokio::test]
async fn refuses_a_corrupt_staged_blob_before_changing_anything() {
	let (base, _, dir) = twins("corrupt-staged-blob", |dir| {
		committed(dir);
		write(dir, "precious", b"precious\n");
		git(dir, &["add", "precious"]);
	});
	let blob = git(&dir, &["rev-parse", ":precious"]);
	let blob = blob.trim();
	let loose = dir.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
	let mut permissions = std::fs::metadata(&loose).unwrap().permissions();
	#[allow(clippy::permissions_set_readonly_false)]
	permissions.set_readonly(false);
	std::fs::set_permissions(&loose, permissions).unwrap();
	std::fs::write(&loose, b"garbage").unwrap();
	let before = state(&dir);
	assert!(push(&dir, &request(&[], "", None, false)).await.is_err());
	assert_eq!(state(&dir), before);
	assert_eq!(std::fs::read(dir.join("precious")).unwrap(), b"precious\n");
	std::fs::remove_dir_all(&base).ok();
}

/// An untracked file whose folder differs only in case from a tracked file the merge deletes: where
/// the filesystem folds case, that file stands in the folder's place until the merge deletes it,
/// and the pop restores the untracked file, as git does (and plainly where it doesn't fold).
#[tokio::test]
async fn pops_untracked_files_in_a_case_folded_deleted_file_s_place_as_git_does() {
	let (base, by_git, by_gitana) = twins("case-folded-deleted", |dir| {
		committed(dir);
		git(dir, &["rm", "-q", "b.txt"]);
		write(dir, "B.TXT/u", b"untracked\n");
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// A tracked name and an untracked one the filesystem treats as one (`é` composed and decomposed on
/// macOS), with everything ignored: the merge would otherwise write the tracked file over the
/// untracked one just restored (an ignored file being expendable to it), so the pop refuses with
/// nothing changed and the stash kept, as review 216 settled for names the filesystem judges one;
/// where the filesystem tells them apart, both are restored, as git restores them.
#[tokio::test]
async fn refuses_a_tracked_and_an_untracked_name_the_filesystem_sees_as_one() {
	let (base, by_git, by_gitana) = twins("unicode-tracked-untracked", |dir| {
		committed(dir);
		write(dir, ".git/info/exclude", b"*\n");
		hand_made_stash(
			dir,
			&[("\u{e9}", b"tracked\n")],
			&[("e\u{301}", b"untracked\n")],
		);
	});
	let probe = base.join("\u{e9}");
	std::fs::write(&probe, b"").unwrap();
	let one_name = base.join("e\u{301}").exists();
	std::fs::remove_file(&probe).unwrap();
	let git_popped = git_raw(&by_git, &["stash", "pop", "-q"]).status.success();
	if one_name {
		assert!(!git_popped);
		let before = state(&by_gitana);
		let error = worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap_err();
		assert!(
			matches!(
				error,
				gitana_worktree::StashPopError::Apply(gitana_worktree::StashApplyError::UntrackedExists(_))
			),
			"{error:?}"
		);
		assert_eq!(state(&by_gitana), before);
	} else {
		assert!(git_popped);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git));
	}
	std::fs::remove_dir_all(&base).ok();
}

/// An untracked file named, as the filesystem judges names, like a folder the merge needs for a
/// tracked file (`é` decomposed, the stash's `é/child` composed), with everything ignored: the
/// merge would delete it to make the folder, so the pop refuses with nothing changed and the stash
/// kept; where the filesystem tells them apart, both are restored, as git restores them.
#[tokio::test]
async fn refuses_an_untracked_file_where_the_merge_needs_a_folder_of_its_name() {
	let (base, by_git, by_gitana) = twins("unicode-folder", |dir| {
		committed(dir);
		write(dir, ".git/info/exclude", b"*\n");
		hand_made_stash(
			dir,
			&[("\u{e9}/child", b"tracked\n")],
			&[("e\u{301}", b"untracked\n")],
		);
	});
	let probe = base.join("\u{e9}");
	std::fs::write(&probe, b"").unwrap();
	let one_name = base.join("e\u{301}").exists();
	std::fs::remove_file(&probe).unwrap();
	let git_popped = git_raw(&by_git, &["stash", "pop", "-q"]).status.success();
	if one_name {
		assert!(!git_popped);
		let before = state(&by_gitana);
		let error = worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap_err();
		assert!(
			matches!(
				error,
				gitana_worktree::StashPopError::Apply(gitana_worktree::StashApplyError::UntrackedExists(_))
			),
			"{error:?}"
		);
		assert_eq!(state(&by_gitana), before);
	} else {
		assert!(git_popped);
		worktree(&by_gitana)
			.stash_pop(&pop_request(0, false))
			.await
			.unwrap();
		assert_eq!(state(&by_gitana), state(&by_git));
	}
	std::fs::remove_dir_all(&base).ok();
}

/// A submodule repository to add, made once beside the twins.
fn submodule_source(dir: &Path) -> std::path::PathBuf {
	let source = dir.parent().unwrap().join("source");
	if !source.exists() {
		std::fs::create_dir_all(&source).unwrap();
		git(&source, &["init", "-q", "-b", "main"]);
		write(&source, "s.txt", b"s\n");
		git(&source, &["add", "."]);
		git(&source, &["commit", "-q", "-m", "s"]);
	}
	source
}

/// A submodule staged where `HEAD` has a file, its checkout holding work (or not): a whole push
/// would put the file back and delete the checkout with it (git deletes it), so it's refused before
/// anything is stored.
#[tokio::test]
async fn refuses_to_remove_a_submodule_checkout_staged_in_a_file_s_place() {
	for with_work in [true, false] {
		let (base, _, dir) = twins("submodule-in-a-file", |dir| {
			committed(dir);
			let source = submodule_source(dir);
			git(dir, &["rm", "-q", "b.txt"]);
			git(
				dir,
				&[
					"-c",
					"protocol.file.allow=always",
					"submodule",
					"add",
					"-q",
					source.to_str().unwrap(),
					"b.txt",
				],
			);
			if with_work {
				write(dir, "b.txt/s.txt", b"edited\n");
				write(dir, "b.txt/untracked", b"precious\n");
			}
			write(dir, "a.txt", b"one!\n");
		});
		let before = state(&dir);
		for untracked in [false, true] {
			let error = push(&dir, &request(&[], "", None, untracked))
				.await
				.unwrap_err();
			assert!(
				matches!(
					&error,
					StashPushError::Worktree(gitana_worktree::WorktreeError::UntrackedOverwrite(path))
						if path.as_bytes() == b"b.txt"
				),
				"{error:?}"
			);
			assert_eq!(state(&dir), before, "{with_work} {untracked}");
		}
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A submodule staged where `HEAD` has a file, its folder emptied (`deinit`): nothing to lose, so
/// the push puts the file back, as git does.
#[tokio::test]
async fn replaces_an_emptied_submodule_folder_as_git_does() {
	let (base, by_git, by_gitana) = twins("submodule-emptied", |dir| {
		committed(dir);
		let source = submodule_source(dir);
		git(dir, &["rm", "-q", "b.txt"]);
		git(
			dir,
			&[
				"-c",
				"protocol.file.allow=always",
				"submodule",
				"add",
				"-q",
				source.to_str().unwrap(),
				"b.txt",
			],
		);
		git(dir, &["submodule", "deinit", "-q", "-f", "b.txt"]);
		write(dir, "a.txt", b"one!\n");
	});
	git(&by_git, &["stash", "-q"]);
	push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// An untracked file in a folder named, as the filesystem judges names, like one the merge creates
/// for a tracked file (`N/u` and `n/new`): one folder where case folds, two where it doesn't, and
/// the pop restores both, as git does.
#[tokio::test]
async fn pops_untracked_files_into_a_folder_the_merge_also_creates_as_git_does() {
	let (base, by_git, by_gitana) = twins("shared-folder", |dir| {
		committed(dir);
		hand_made_stash(dir, &[("n/new", b"tracked\n")], &[("N/u", b"untracked\n")]);
	});
	git(&by_git, &["stash", "pop", "-q"]);
	worktree(&by_gitana)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// The object store already holds a corrupt copy at the hash of what's saved (an unstaged edit, an
/// untracked file): the write repairs it, so the stash is sound and the pop brings both back,
/// where git saves the corrupt copy and loses them.
#[tokio::test]
async fn saves_over_a_corrupt_copy_already_in_the_store() {
	let (base, _, dir) = twins("corrupt-existing", committed);
	for content in [&b"edited\n"[..], b"untracked\n"] {
		let oid = git_input(&dir, &["hash-object", "--stdin"], content);
		let oid = oid.trim();
		let loose = dir.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
		std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
		std::fs::write(&loose, b"garbage").unwrap();
	}
	write(&dir, "a.txt", b"edited\n");
	write(&dir, "u.txt", b"untracked\n");
	push(&dir, &request(&[], "", None, true))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(git(&dir, &["cat-file", "-p", "stash:a.txt"]), "edited\n");
	assert_eq!(
		git(&dir, &["cat-file", "-p", "stash^3:u.txt"]),
		"untracked\n"
	);
	worktree(&dir)
		.stash_pop(&pop_request(0, false))
		.await
		.unwrap();
	assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"edited\n");
	assert_eq!(std::fs::read(dir.join("u.txt")).unwrap(), b"untracked\n");
	std::fs::remove_dir_all(&base).ok();
}

/// A submodule staged in the place of a tracked folder (`d`, where `HEAD` has `d/c.txt`), its
/// checkout holding its own `c.txt`, edited: a whole push would write `HEAD`'s `d/c.txt` into the
/// checkout over the edit, so it's refused before anything is stored, as git refuses it (git's
/// save fails on the path appearing as both a file and a folder).
#[tokio::test]
async fn refuses_to_write_into_a_submodule_checkout_staged_in_a_folder_s_place() {
	let (base, by_git, by_gitana) = twins("submodule-in-a-folder", |dir| {
		committed(dir);
		let source = dir.parent().unwrap().join("folder-source");
		if !source.exists() {
			std::fs::create_dir_all(&source).unwrap();
			git(&source, &["init", "-q", "-b", "main"]);
			write(&source, "c.txt", b"the submodule's\n");
			git(&source, &["add", "."]);
			git(&source, &["commit", "-q", "-m", "s"]);
		}
		git(dir, &["rm", "-q", "-r", "d"]);
		git(
			dir,
			&[
				"-c",
				"protocol.file.allow=always",
				"submodule",
				"add",
				"-q",
				source.to_str().unwrap(),
				"d",
			],
		);
		write(dir, "d/c.txt", b"edited in the submodule\n");
		write(dir, "a.txt", b"one!\n");
	});
	let before = state(&by_gitana);
	assert!(!git_raw(&by_git, &["stash", "-q"]).status.success());
	let error = push(&by_gitana, &request(&[], "", None, false))
		.await
		.unwrap_err();
	assert!(
		matches!(
			&error,
			StashPushError::Worktree(gitana_worktree::WorktreeError::UntrackedOverwrite(path))
				if path.as_bytes() == b"d"
		),
		"{error:?}"
	);
	assert_eq!(state(&by_gitana), before);
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// A whole push during a merge left uncommitted with an autostash (`merge --autostash
/// --no-commit`): finishing the reset saves the autostash on top, as git does, and the stash
/// pushed is returned at its place below it.
#[tokio::test]
async fn reports_its_place_below_a_merge_autostash_as_git_leaves_them() {
	let (base, by_git, by_gitana) = twins("merge-autostash", |dir| {
		committed(dir);
		git(dir, &["checkout", "-q", "-b", "other"]);
		write(dir, "b.txt", b"other\n");
		git(dir, &["commit", "-q", "-am", "other"]);
		git(dir, &["checkout", "-q", "main"]);
		write(dir, "d/c.txt", b"main\n");
		git(dir, &["commit", "-q", "-am", "main"]);
		write(dir, "a.txt", b"dirty\n");
		git(
			dir,
			&[
				"merge",
				"-q",
				"--no-ff",
				"--no-commit",
				"--autostash",
				"other",
			],
		);
	});
	assert!(by_gitana.join(".git/MERGE_AUTOSTASH").exists());
	git(&by_git, &["stash", "-q"]);
	let pushed = worktree(&by_gitana)
		.stash_push(&request(&[], "", None, false), &TestIdentity)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(pushed.entry.index, 1);
	assert_eq!(
		pushed.entry.commit.to_hex(),
		git(&by_gitana, &["rev-parse", "stash@{1}"]).trim()
	);
	assert_eq!(stash_list(&by_gitana), stash_list(&by_git));
	assert_eq!(state(&by_gitana), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}
