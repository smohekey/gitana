//! The stash list, a stash's drop and store, and abbreviated ids, cross-checked against stock git:
//! twin repositories made identically by git, one changed by gitana and one by git, compared byte
//! for byte (`logs/refs/stash`, `refs/stash`).
#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::{ObjectId, Sha1};
use gitana_object_store::ObjectStore;
use gitana_repository::{Repository, RepositoryError};

const WHEN: &str = "1700000000 +0000";

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn repo(work: &Path) -> Repository<LocalFileStore, Sha1> {
	Repository::new(ObjectStore::new(LocalFileStore::from_dir(open_dir(
		work.join(".git"),
	))))
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-stash-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// git in `dir`, with a home of its own, no system config and a fixed identity and time.
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

/// A repository with three stashes, made by git: the newest a `-m` with two lines.
fn three_stashes(dir: &Path) {
	std::fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	std::fs::write(dir.join("a.txt"), "one\n").unwrap();
	git(dir, &["add", "a.txt"]);
	git(dir, &["commit", "-q", "-m", "base"]);
	for (content, message) in [
		("two\n", None),
		("three\n", None),
		("four\n", Some("two\n  lines")),
	] {
		std::fs::write(dir.join("a.txt"), content).unwrap();
		match message {
			Some(message) => git(dir, &["stash", "push", "-q", "-m", message]),
			None => git(dir, &["stash", "push", "-q"]),
		};
	}
}

fn read(dir: &Path, path: &str) -> Option<Vec<u8>> {
	std::fs::read(dir.join(".git").join(path)).ok()
}

fn stash_ref(dir: &Path) -> String {
	String::from_utf8(git_raw(dir, &["rev-parse", "--verify", "-q", "refs/stash"]).stdout).unwrap()
}

#[tokio::test]
async fn lists_git_s_stashes_as_git_does() {
	let base = unique_tmp("list");
	let dir = base.join("repo");
	three_stashes(&dir);
	let listed: Vec<String> = repo(&dir)
		.stash_list()
		.await
		.unwrap()
		.into_iter()
		.map(|entry| {
			format!(
				"stash@{{{}}} {} {}",
				entry.index, entry.commit, entry.message
			)
		})
		.collect();
	let expected: Vec<String> = git(&dir, &["stash", "list", "--format=%gd %H %gs"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(listed, expected);
	assert_eq!(
		repo(&dir).refs().reflog("refs/stash").await.unwrap().len(),
		3
	);
	assert!(
		repo(&dir)
			.refs()
			.reflog("refs/nope")
			.await
			.unwrap()
			.is_empty()
	);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn drops_each_stash_as_git_does() {
	for packed in [false, true] {
		for index in 0..3 {
			let base = unique_tmp("drop");
			let (by_git, by_gitana) = (base.join("git"), base.join("gitana"));
			three_stashes(&by_git);
			three_stashes(&by_gitana);
			if packed {
				git(&by_git, &["pack-refs", "--all"]);
				git(&by_gitana, &["pack-refs", "--all"]);
			}
			let expected = stash_ref(&by_gitana);
			let commit = git(&by_gitana, &["rev-parse", &format!("stash@{{{index}}}")]);
			git(
				&by_git,
				&["stash", "drop", "-q", &format!("stash@{{{index}}}")],
			);
			let dropped = repo(&by_gitana).drop_stash(index, None).await.unwrap();
			assert_eq!(
				dropped.map(|id| id.to_hex()),
				Some(commit.trim().to_owned())
			);
			assert_eq!(
				read(&by_gitana, "logs/refs/stash"),
				read(&by_git, "logs/refs/stash"),
				"packed {packed}, stash@{{{index}}}"
			);
			assert_eq!(stash_ref(&by_gitana), stash_ref(&by_git));
			assert!(index == 0 || stash_ref(&by_gitana) == expected);
			std::fs::remove_dir_all(&base).ok();
		}
	}
}

#[tokio::test]
async fn dropping_the_last_stash_removes_the_ref_and_its_log_as_git_does() {
	let base = unique_tmp("drop-last");
	let (by_git, by_gitana) = (base.join("git"), base.join("gitana"));
	three_stashes(&by_git);
	three_stashes(&by_gitana);
	for _ in 0..3 {
		git(&by_git, &["stash", "drop", "-q"]);
		repo(&by_gitana).drop_stash(0, None).await.unwrap().unwrap();
	}
	assert_eq!(read(&by_git, "logs/refs/stash"), None);
	assert_eq!(read(&by_gitana, "logs/refs/stash"), None);
	assert_eq!(stash_ref(&by_gitana), "");
	assert_eq!(stash_ref(&by_git), "");
	assert_eq!(repo(&by_gitana).drop_stash(0, None).await.unwrap(), None);
	assert!(repo(&by_gitana).stash_list().await.unwrap().is_empty());
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn a_drop_checks_the_stash_it_expects() {
	let base = unique_tmp("drop-expected");
	let dir = base.join("repo");
	three_stashes(&dir);
	let before = read(&dir, "logs/refs/stash");
	let other = ObjectId::<Sha1>::from_hex(git(&dir, &["rev-parse", "stash@{1}"]).trim()).unwrap();
	let error = repo(&dir).drop_stash(0, Some(other)).await.unwrap_err();
	assert!(
		matches!(error, RepositoryError::RefMoved { .. }),
		"{error:?}"
	);
	assert_eq!(read(&dir, "logs/refs/stash"), before);
	assert_eq!(repo(&dir).drop_stash(7, None).await.unwrap(), None);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn stores_a_stash_as_git_stash_store_does() {
	let base = unique_tmp("store");
	let (by_git, by_gitana) = (base.join("git"), base.join("gitana"));
	for dir in [&by_git, &by_gitana] {
		three_stashes(dir);
		std::fs::write(dir.join("a.txt"), "five\n").unwrap();
	}
	let commit = git(&by_git, &["stash", "create"]);
	let commit = commit.trim();
	assert_eq!(git(&by_gitana, &["stash", "create"]).trim(), commit);
	let message = "  On main:\tspaced\n\n  out  ";
	git(&by_git, &["stash", "store", "-m", message, commit]);
	repo(&by_gitana)
		.store_stash(
			ObjectId::from_hex(commit).unwrap(),
			&format!("C <c@e> {WHEN}"),
			message,
		)
		.await
		.unwrap();
	assert_eq!(
		read(&by_gitana, "logs/refs/stash"),
		read(&by_git, "logs/refs/stash")
	);
	assert_eq!(stash_ref(&by_gitana), stash_ref(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn abbreviates_as_git_does() {
	let base = unique_tmp("abbrev");
	let dir = base.join("repo");
	three_stashes(&dir);
	let head = ObjectId::<Sha1>::from_hex(git(&dir, &["rev-parse", "HEAD"]).trim()).unwrap();
	let short = |dir: &Path| {
		git(dir, &["rev-parse", "--short", "HEAD"])
			.trim()
			.to_owned()
	};
	assert_eq!(repo(&dir).abbreviate(head).await.unwrap(), short(&dir));
	git(&dir, &["config", "core.abbrev", "10"]);
	assert_eq!(repo(&dir).abbreviate(head).await.unwrap(), short(&dir));
	git(&dir, &["config", "core.abbrev", "no"]);
	assert_eq!(repo(&dir).abbreviate(head).await.unwrap(), head.to_hex());
	git(&dir, &["config", "--unset", "core.abbrev"]);
	// Past 2^14 packed objects git's automatic length grows to 8.
	let mut import = String::new();
	for i in 0..17_000 {
		let content = format!("blob {i}");
		import.push_str(&format!("blob\ndata {}\n{content}\n", content.len()));
	}
	let mut child = Command::new("git")
		.args(["-C", dir.to_str().unwrap(), "fast-import", "--quiet"])
		.stdin(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	use std::io::Write as _;
	child
		.stdin
		.take()
		.unwrap()
		.write_all(import.as_bytes())
		.unwrap();
	assert!(child.wait().unwrap().success());
	git(&dir, &["repack", "-q", "-d"]);
	let expected = short(&dir);
	assert_eq!(expected.len(), 8);
	assert_eq!(repo(&dir).abbreviate(head).await.unwrap(), expected);
	std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn lists_a_stash_s_files_as_git_stash_show_does() {
	let base = unique_tmp("files");
	let dir = base.join("repo");
	std::fs::create_dir_all(dir.join("d")).unwrap();
	git(&dir, &["init", "-q", "-b", "main"]);
	for (path, content) in [
		("a.txt", "one\n"),
		("b.txt", "two\n"),
		("d/c.txt", "three\n"),
	] {
		std::fs::write(dir.join(path), content).unwrap();
	}
	git(&dir, &["add", "."]);
	git(&dir, &["commit", "-q", "-m", "base"]);
	std::fs::write(dir.join("a.txt"), "one!\n").unwrap();
	std::fs::remove_file(dir.join("d/c.txt")).unwrap();
	std::fs::write(dir.join("new.txt"), "new\n").unwrap();
	git(&dir, &["add", "new.txt"]);
	std::fs::write(dir.join("untracked.txt"), "u\n").unwrap();
	git(&dir, &["stash", "push", "-q", "-u"]);
	for include_untracked in [false, true] {
		let mut args = vec!["stash", "show", "--name-only"];
		if include_untracked {
			args.push("--include-untracked");
		}
		let expected: Vec<String> = git(&dir, &args).lines().map(str::to_owned).collect();
		let files: Vec<String> = repo(&dir)
			.stash_files(0, include_untracked)
			.await
			.unwrap()
			.unwrap()
			.iter()
			.map(|path| path.to_string())
			.collect();
		assert_eq!(files, expected, "include_untracked {include_untracked}");
	}
	assert_eq!(repo(&dir).stash_files(1, false).await.unwrap(), None);
	std::fs::remove_dir_all(&base).ok();
}
