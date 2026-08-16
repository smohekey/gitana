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
			.status(None)
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
			.status(None)
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
				.status(None)
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
				.status(None)
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
			.status(None)
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
			.status(None)
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
			.status(None)
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
			.status(None)
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

#[tokio::test]
async fn status_folds_ignorecase_for_gitignore() {
	// git consults `core.ignoreCase` when matching `.gitignore` for untracked detection. A rule in UPPER
	// case only matches a lower-case file when folded, so the file's untracked-ness flips with the flag —
	// and gitana's `status` must track git in both directions (previously it matched case-sensitively).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ignorecase");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.LOG\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
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
	std::fs::write(work.join("debug.log"), b"log\n").unwrap();

	let status = || async {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		sorted(
			&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
				.status(None)
				.await
				.unwrap()
				.porcelain_v1(),
		)
	};

	// ignoreCase=true: `*.LOG` folds onto `debug.log` → git omits it as ignored; so must we.
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	assert!(
		!git(&["-C", w, "status", "--porcelain=v1"]).contains("debug.log"),
		"sanity: git folds `*.LOG` onto debug.log and ignores it"
	);
	assert_eq!(
		status().await,
		sorted(&git(&["-C", w, "status", "--porcelain=v1"])),
		"ignoreCase=true: debug.log must be folded-ignored, matching git"
	);

	// ignoreCase=false: `*.LOG` does not match `debug.log` → git lists it untracked; so must we.
	git(&["-C", w, "config", "core.ignoreCase", "false"]);
	assert!(
		git(&["-C", w, "status", "--porcelain=v1"]).contains("debug.log"),
		"sanity: case-sensitive, debug.log is untracked"
	);
	assert_eq!(
		status().await,
		sorted(&git(&["-C", w, "status", "--porcelain=v1"])),
		"ignoreCase=false: debug.log must read untracked, matching git"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_folds_ignorecase_tracked_membership() {
	// Under `core.ignoreCase`, git matches a working-tree entry to a tracked index path case-folded: a
	// disk `foo.txt` counts as the tracked `Foo.txt` (not untracked). The differential compare drives git
	// and gitana over the same working tree, so it holds whatever the filesystem's own case behaviour.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ignorecase-tracked");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("Foo.txt"), b"x\n").unwrap();
	git(&["-C", w, "add", "Foo.txt"]);
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
	// Re-create the file under a different case; the index still holds `Foo.txt`.
	std::fs::remove_file(work.join("Foo.txt")).unwrap();
	std::fs::write(work.join("foo.txt"), b"x\n").unwrap();

	let status = || async {
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		sorted(
			&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
				.status(None)
				.await
				.unwrap()
				.porcelain_v1(),
		)
	};

	for value in ["true", "false"] {
		git(&["-C", w, "config", "core.ignoreCase", value]);
		assert_eq!(
			status().await,
			sorted(&git(&["-C", w, "status", "--porcelain=v1"])),
			"ignoreCase={value}: tracked-membership folding must match git"
		);
	}
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_honors_info_exclude() {
	// git consults `.git/info/exclude` for untracked detection everywhere; gitana's `status` reads it
	// internally over the `.git` store (no caller help), so `status(None)` must still honour it.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-info-exclude");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
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
	std::fs::write(git_dir.join("info").join("exclude"), b"*.tmp\n").unwrap();
	std::fs::write(work.join("scratch.tmp"), b"t\n").unwrap();
	std::fs::write(work.join("keep.txt"), b"k\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		!theirs.iter().any(|l| l.contains("scratch.tmp"))
			&& theirs.iter().any(|l| l.contains("keep.txt")),
		"sanity: git honours .git/info/exclude (scratch.tmp omitted, keep.txt untracked): {theirs:?}"
	);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"status must honour .git/info/exclude, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_seeds_excludes_file() {
	// The global `core.excludesFile` lives outside the worktree, so the caller resolves its content and
	// passes it in. Given that content, `status` must seed it as the lowest-priority exclude level and
	// match git (which reads the file itself). The excludes file is kept inside `.git` so neither scan
	// lists it as untracked.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-excludes-file");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
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
	let excludes = git_dir.join("custom-excludes");
	std::fs::write(&excludes, b"*.bak\n").unwrap();
	git(&[
		"-C",
		w,
		"config",
		"core.excludesFile",
		excludes.to_str().unwrap(),
	]);
	std::fs::write(work.join("old.bak"), b"b\n").unwrap();
	std::fs::write(work.join("keep.txt"), b"k\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		!theirs.iter().any(|l| l.contains("old.bak")) && theirs.iter().any(|l| l.contains("keep.txt")),
		"sanity: git honours core.excludesFile (old.bak omitted, keep.txt untracked): {theirs:?}"
	);

	let content = std::fs::read_to_string(&excludes).unwrap();
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(Some(&content))
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"status must seed the resolved excludesFile content, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_honors_worktree_config_ignorecase_override() {
	// git honours a per-worktree `core.ignoreCase` override in `config.worktree` (with
	// `extensions.worktreeConfig`) for untracked detection (probed vs git 2.55). status must read that
	// override, not just the common value — reading the wrong one could fold-hide an untracked file from a
	// `worktree remove` safety check and delete it.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-wtcfg-ignorecase");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.LOG\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
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
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&["-C", w, "config", "core.ignoreCase", "true"]); // common: fold
	git(&["-C", w, "config", "--worktree", "core.ignoreCase", "false"]); // override: no fold
	std::fs::write(work.join("debug.log"), b"log\n").unwrap();

	// git honours the override (false): `debug.log` is NOT folded onto `*.LOG`, so it reads untracked.
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		theirs.iter().any(|l| l.contains("debug.log")),
		"sanity: git honours the false worktree override (debug.log untracked): {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"status must honour the per-worktree core.ignoreCase override, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_omits_untracked_dir_with_only_ignored_content() {
	// git omits an untracked directory whose *entire* content is ignored (here by `.git/info/exclude`),
	// but collapses one with any non-ignored content to `dir/`. gta must match — not blindly collapse an
	// all-ignored directory to `?? d/` (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ignored-dir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
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
	std::fs::write(git_dir.join("info").join("exclude"), b"*.log\n").unwrap();
	// `d/` holds only ignored content → omitted; `e/` has a non-ignored file → collapsed to `e/`.
	std::fs::create_dir_all(work.join("d")).unwrap();
	std::fs::write(work.join("d/only.log"), b"x\n").unwrap();
	std::fs::create_dir_all(work.join("e")).unwrap();
	std::fs::write(work.join("e/keep.txt"), b"y\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		!theirs.iter().any(|l| l.contains("d/")) && theirs.iter().any(|l| l.contains("e/")),
		"sanity: git omits the all-ignored d/ and shows e/: {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"an all-ignored untracked directory must be omitted, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_bare_ignorecase_fails_closed_on_includes() {
	// A bare `WorkTree` (no frontend-merged config — the linked-worktree removal-safety path) does not
	// process `[include]`, so a direct `core.ignoreCase=true` cannot be trusted when an include could
	// override it. It must fail closed to `false` (do not fold) rather than fold-hide an untracked file.
	// Here the include resolves `core.ignoreCase` to `false`, so git does not fold `debug.log` onto
	// `*.LOG` and shows it; the fail-closed path reaches the same answer, where blindly trusting the direct
	// `true` would wrongly hide it.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-ic-include");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.LOG\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
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
	// Local `core.ignoreCase=true`, but an include overrides it to false.
	git(&["-C", w, "config", "core.ignoreCase", "true"]);
	std::fs::write(git_dir.join("extra.cfg"), "[core]\n\tignoreCase = false\n").unwrap();
	std::fs::write(
		git_dir.join("config"),
		format!(
			"{}[include]\n\tpath = extra.cfg\n",
			std::fs::read_to_string(git_dir.join("config")).unwrap()
		),
	)
	.unwrap();
	std::fs::write(work.join("debug.log"), b"log\n").unwrap();

	// git resolves the include → ignoreCase false → debug.log not folded, shown untracked.
	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		theirs.iter().any(|l| l.contains("debug.log")),
		"sanity: git's include resolves ignoreCase to false (debug.log shown): {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert!(
		ours.iter().any(|l| l.contains("debug.log")),
		"an include present → fail closed to no-fold → debug.log shown, got {ours:?}"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_shows_embedded_repo_with_only_ignored_content() {
	// A valid untracked embedded git repository must show `?? sub/` even when its own content is entirely
	// ignored by the outer repo — git lists it opaquely and never descends (probed vs git 2.55). Omitting
	// it would let a default `worktree remove` recursively delete the nested repo.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-embedded-repo");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.log\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
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
	// A real embedded repo whose only extra content is ignored.
	std::fs::create_dir_all(work.join("sub")).unwrap();
	git(&["-C", work.join("sub").to_str().unwrap(), "init", "-q"]);
	std::fs::write(work.join("sub/only.log"), b"x\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		theirs.iter().any(|l| l.contains("sub/")),
		"sanity: git shows the embedded repo as ?? sub/: {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"a valid embedded repo with only-ignored content must still be shown, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_handles_unreadable_untracked_dir_nonfatally() {
	// An unreadable untracked directory must not make status fatal: git warns and completes (exit 0),
	// omitting it from the porcelain (probed vs git 2.55). The differential compare is robust to whether
	// the test runs as root (where the dir stays readable).
	use std::os::unix::fs::PermissionsExt;
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-unreadable-dir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
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
	std::fs::create_dir_all(work.join("noread")).unwrap();
	std::fs::write(work.join("noread/f"), b"x\n").unwrap();
	std::fs::write(work.join("keep.txt"), b"k\n").unwrap();
	std::fs::set_permissions(work.join("noread"), std::fs::Permissions::from_mode(0o000)).unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	// Must return Ok (not a fatal PermissionDenied), matching git's exit 0.
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.expect("status must not fail on an unreadable untracked directory")
			.porcelain_v1(),
	);
	// Restore permissions before comparing/cleanup so the temp dir can be removed.
	std::fs::set_permissions(work.join("noread"), std::fs::Permissions::from_mode(0o755)).unwrap();
	assert_eq!(
		ours, theirs,
		"status must handle the unreadable directory like git (omit it, non-fatal)"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn status_shows_gitfile_backed_embedded_repo() {
	// A gitfile-backed embedded repo — `sub/.git` is a *file* pointing to a separate gitdir (a linked
	// worktree / submodule) — whose visible content is entirely ignored must still show `?? sub/` (probed
	// vs git 2.55). Omitting it would let a default `worktree remove` delete the nested repo.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-gitfile-repo");
	let gitdir_ext = unique_tmp("status-gitfile-ext"); // the embedded repo's gitdir, OUTSIDE the worktree
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join(".gitignore"), b"*.log\n").unwrap();
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
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
	git(&[
		"init",
		"--separate-git-dir",
		gitdir_ext.to_str().unwrap(),
		"-q",
		work.join("sub").to_str().unwrap(),
	]);
	std::fs::write(work.join("sub/only.log"), b"x\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		theirs.iter().any(|l| l.contains("sub/")),
		"sanity: git shows the gitfile-backed embedded repo as ?? sub/: {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.unwrap()
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"a gitfile-backed embedded repo with only-ignored content must still be shown, matching git"
	);
	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gitdir_ext).ok();
}

#[tokio::test]
async fn status_tolerates_gitignore_directory_in_untracked_dir() {
	// A `.gitignore` that is a *directory* inside an otherwise-untracked directory must not abort status:
	// git treats it as contributing no rules and reports the parent `?? dir/` (probed vs git 2.55).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("status-gitignore-dir");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
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
	std::fs::create_dir_all(work.join("d2/.gitignore")).unwrap(); // `.gitignore` is a directory
	std::fs::write(work.join("d2/keep.txt"), b"y\n").unwrap();

	let theirs = sorted(&git(&["-C", w, "status", "--porcelain=v1"]));
	assert!(
		theirs.iter().any(|l| l.contains("d2/")),
		"sanity: git tolerates the .gitignore directory and shows ?? d2/: {theirs:?}"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let ours = sorted(
		&WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
			.status(None)
			.await
			.expect("status must not abort on a .gitignore directory")
			.porcelain_v1(),
	);
	assert_eq!(
		ours, theirs,
		"an unusable .gitignore must contribute no rules, not abort status, matching git"
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
	// A per-call sequence number keeps every temp dir distinct even for a reused tag, so tests running
	// in parallel threads never race on `remove_dir_all`/`create_dir_all` for the same path (which
	// surfaced as a transient `File exists`). Matches the `git_diff`/`git_submodule` harnesses.
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
	let probe = unique_tmp("probe-status");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
