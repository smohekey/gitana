#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

#[tokio::test]
async fn status_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	std::fs::write(work.join("b.txt"), b"2\n").unwrap();
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/c.txt"), b"3\n").unwrap();
	git(&["-C", w, "add", "."]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	// Unstaged modification, staged modification, unstaged deletion, untracked.
	std::fs::write(work.join("a.txt"), b"1 changed\n").unwrap();
	std::fs::write(work.join("b.txt"), b"2 new\n").unwrap();
	git(&["-C", w, "add", "b.txt"]);
	std::fs::remove_file(work.join("dir/c.txt")).unwrap();
	std::fs::write(work.join("new.txt"), b"x\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));

	assert_eq!(ours, theirs, "our status must match git status --porcelain");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_with_gitignore_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ignore");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	std::fs::write(work.join(".gitignore"), b"*.log\nbuild/\n").unwrap();
	git(&["-C", w, "add", "a.txt", ".gitignore"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	// Ignored (omitted), untracked directory (collapsed), untracked file, modified.
	std::fs::write(work.join("debug.log"), b"log\n").unwrap();
	std::fs::create_dir_all(work.join("build")).unwrap();
	std::fs::write(work.join("build/out.o"), b"obj\n").unwrap();
	std::fs::create_dir_all(work.join("newdir")).unwrap();
	std::fs::write(work.join("newdir/x.txt"), b"x\n").unwrap();
	std::fs::write(work.join("keep.txt"), b"k\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a changed\n").unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));

	assert_eq!(
		ours, theirs,
		"status must match git with .gitignore + untracked dir"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_honors_core_filemode() {
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-filemode");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("s.sh"), b"echo hi\n").unwrap();
	git(&["-C", w, "add", "s.sh"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	let status = || async {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		sorted(
			&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
				.status()
				.await
				.unwrap()
				.porcelain_v1(),
		)
	};

	// Flip the executable bit only (content unchanged).
	std::fs::set_permissions(work.join("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

	// core.fileMode=true (the default): git reports the exec-bit change as modified; so must we.
	git(&["-C", w, "config", "core.fileMode", "true"]);
	assert_eq!(
		status().await,
		sorted(&git(&["-C", w, "status", "--porcelain=v1"])),
		"fileMode=true: exec-bit change should be modified, matching git"
	);
	assert!(status().await.iter().any(|l| l.contains("s.sh")));

	// core.fileMode=false: git ignores the exec-bit change (clean); so must we.
	git(&["-C", w, "config", "core.fileMode", "false"]);
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).is_empty(),
		"sanity: git is clean"
	);
	assert_eq!(
		status().await,
		sorted(&git(&["-C", w, "status", "--porcelain=v1"])),
		"fileMode=false: exec-bit-only change must be clean, matching git"
	);
	assert!(
		status().await.is_empty(),
		"fileMode=false: worktree must read clean"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_filemode_worktree_override_numeric_is_true() {
	// A worktree-config override of core.fileMode using git's numeric boolean (`2` = true) must NOT be dropped
	// in favour of a common `false` — that would report a modified checkout as clean. Our boolean grammar does
	// not parse `2`, so we fail safe to true (honour the exec bit), matching git (every nonzero numeric = true).
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-filemode-num");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("s.sh"), b"echo hi\n").unwrap();
	git(&["-C", w, "add", "s.sh"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&["-C", w, "config", "core.fileMode", "false"]); // common: ignore exec bit
	git(&["-C", w, "config", "--worktree", "core.fileMode", "2"]); // override: numeric true
	std::fs::set_permissions(work.join("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
	// git honours the numeric override and reports the exec-bit change as modified.
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).contains("s.sh"),
		"sanity: git treats the numeric override as true (modified)"
	);

	let status = || async {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		sorted(
			&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
				.status()
				.await
				.unwrap()
				.porcelain_v1(),
		)
	};
	assert!(
		status().await.iter().any(|l| l.contains("s.sh")),
		"numeric override 2 must be treated as true (modified)"
	);

	// A numeric *false* spelling (0k = zero) must be treated as false — a git-clean worktree, matching git.
	git(&["-C", w, "config", "--worktree", "core.fileMode", "0k"]);
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).is_empty(),
		"sanity: git treats 0k as false (clean)"
	);
	assert!(
		status().await.is_empty(),
		"numeric override 0k must be treated as false (clean), got {:?}",
		status().await
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_fails_closed_on_filemode_include() {
	// A local `[include]` can override core.fileMode; we don't process includes, so a local `false` cannot be
	// trusted — we fail closed to true (honour the exec bit). git resolves the include to true too, so both
	// report the exec-bit change as modified. (Trusting the un-included `false` would delete a modified checkout.)
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-include");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("s.sh"), b"echo hi\n").unwrap();
	git(&["-C", w, "add", "s.sh"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "config", "core.fileMode", "false"]); // local says ignore the exec bit …
	// … but an include overrides it back to true.
	std::fs::write(git_dir.join("extra.cfg"), "[core]\n\tfileMode = true\n").unwrap();
	std::fs::write(
		git_dir.join("config"),
		format!(
			"{}[include]\n\tpath = extra.cfg\n",
			std::fs::read_to_string(git_dir.join("config")).unwrap()
		),
	)
	.unwrap();
	std::fs::set_permissions(work.join("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
	// git resolves the include → fileMode true → modified.
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).contains("s.sh"),
		"sanity: git's include resolves fileMode to true (modified)"
	);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert!(
		ours.iter().any(|l| l.contains("s.sh")),
		"an include present → fail closed to modified, got {ours:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_fails_closed_on_worktree_config_include() {
	// An include *inside* config.worktree can override core.fileMode; we don't process it, so a direct false
	// there cannot be trusted either — fail closed to true, matching git (which resolves the include to true).
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-wtcfg-include");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("s.sh"), b"echo hi\n").unwrap();
	git(&["-C", w, "add", "s.sh"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&["-C", w, "config", "core.fileMode", "false"]); // common: ignore exec bit
	// config.worktree directly says false, but includes a file overriding it to true.
	std::fs::write(git_dir.join("wt-extra.cfg"), "[core]\n\tfileMode = true\n").unwrap();
	std::fs::write(
		git_dir.join("config.worktree"),
		"[core]\n\tfileMode = false\n[include]\n\tpath = wt-extra.cfg\n",
	)
	.unwrap();
	std::fs::set_permissions(work.join("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).contains("s.sh"),
		"sanity: git resolves the config.worktree include to fileMode true (modified)"
	);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert!(
		ours.iter().any(|l| l.contains("s.sh")),
		"an include in config.worktree → fail closed to modified, got {ours:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_fails_closed_on_malformed_worktree_config() {
	// With extensions.worktreeConfig, common fileMode=false, and a *malformed* config.worktree, we cannot
	// establish false — fail closed to true (honour the exec bit). git errors here; the key point is that we do
	// not silently fall back to the common false and treat a modified checkout as clean.
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-badwtcfg");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("s.sh"), b"echo hi\n").unwrap();
	git(&["-C", w, "add", "s.sh"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&["-C", w, "config", "core.fileMode", "false"]);
	// A malformed per-worktree config (unclosed section header).
	std::fs::write(
		git_dir.join("config.worktree"),
		b"[core\n\tfileMode = false\n",
	)
	.unwrap();
	std::fs::set_permissions(work.join("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert!(
		ours.iter().any(|l| l.contains("s.sh")),
		"a malformed config.worktree → fail closed to modified, got {ours:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_ignores_skip_worktree_entries() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-skipwt");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	std::fs::write(work.join("b.txt"), b"b\n").unwrap();
	git(&["-C", w, "add", "a.txt", "b.txt"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);

	// Mark b.txt skip-worktree (as sparse checkout does) and remove it from the working tree — git then
	// ignores it entirely, so status stays clean even though the file is absent.
	git(&["-C", w, "update-index", "--skip-worktree", "b.txt"]);
	std::fs::remove_file(work.join("b.txt")).unwrap();
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).is_empty(),
		"sanity: git ignores the skip-worktree file's absence"
	);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status()
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert!(
		ours.is_empty(),
		"skip-worktree entry must not read as deleted, got {ours:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

fn sorted(porcelain: &str) -> Vec<String> {
	let mut lines: Vec<String> = porcelain.lines().map(str::to_owned).collect();
	lines.sort();
	lines
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

fn unique_tmp(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("gitana-worktree-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	let probe = unique_tmp("probe-status");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
