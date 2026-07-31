#![cfg(unix)]

//! `gta ls-files` checked byte-for-byte against stock `git` as the oracle: pathspec filtering, the
//! `-c`/`-s`/`-o`/`-m`/`-d` selection sets and their combinations, cwd-relative output (and
//! `--full-name`), `-z`, C-style path quoting, `--error-unmatch`, and unmerged / sparse index
//! states. `ls-files` is read-only, and `gta` reads git's byte-compatible SHA-1 repository, so a
//! single git-built repo is driven by both tools and their output compared directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `git -C dir <args>`.
fn git_raw(dir: &Path, args: &[&str]) -> Output {
	let mut full = vec!["-C", dir.to_str().unwrap()];
	full.extend_from_slice(args);
	Command::new("git").args(&full).output().expect("run git")
}

/// Run `gta -C dir <args>`.
fn gta_raw(dir: &Path, args: &[&str]) -> Output {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir.to_str().unwrap()])
		.args(args)
		.output()
		.expect("run gta")
}

/// Assert a `git` invocation succeeded (used for repository setup).
fn git_ok(dir: &Path, args: &[&str]) {
	let out = git_raw(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
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

/// A fresh SHA-1 repo (git's default format, so stock `git` is the oracle) with a fixed identity.
fn git_repo(tag: &str) -> PathBuf {
	let dir = unique_tmp(tag);
	git_ok(&dir, &["init", "-q", "-b", "main", dir.to_str().unwrap()]);
	git_ok(&dir, &["config", "user.email", "t@e"]);
	git_ok(&dir, &["config", "user.name", "T"]);
	dir
}

/// Drive both tools with the same `args` in the same directory (`repo`/`subrel`) and assert identical
/// raw stdout bytes and exit codes — the byte comparison catches `-z` NUL separators and octal quoting.
fn assert_same(repo: &Path, subrel: &str, args: &[&str]) {
	let dir = if subrel.is_empty() {
		repo.to_path_buf()
	} else {
		repo.join(subrel)
	};
	let full: Vec<&str> = std::iter::once("ls-files")
		.chain(args.iter().copied())
		.collect();
	let git_out = git_raw(&dir, &full);
	let gta_out = gta_raw(&dir, &full);
	assert_eq!(
		gta_out.stdout,
		git_out.stdout,
		"stdout mismatch for `ls-files {args:?}` in {subrel:?}\n git: {:?}\n gta: {:?}",
		String::from_utf8_lossy(&git_out.stdout),
		String::from_utf8_lossy(&gta_out.stdout),
	);
	assert_eq!(
		gta_out.status.code().map(|c| c != 0),
		git_out.status.code().map(|c| c != 0),
		"exit-nonzero mismatch for `ls-files {args:?}` in {subrel:?}"
	);
}

/// Pathspec filtering, the selection sets and their combinations, `-z`, quoting, cwd-relative output
/// and `--full-name`, and `--error-unmatch` — all against a single rich working tree.
#[test]
fn ls_files_matches_git() {
	let repo = git_repo("lsf");
	std::fs::create_dir_all(repo.join("src")).unwrap();
	std::fs::create_dir_all(repo.join("sub")).unwrap();
	std::fs::create_dir_all(repo.join("vendor")).unwrap();
	for (p, c) in [
		("README.md", "r\n"),
		("src/lib.rs", "l\n"),
		("src/main.rs", "m\n"),
		("sub/a.txt", "a\n"),
		("vendor/x.rs", "x\n"),
		("mod.txt", "a\n"),
		("del.txt", "a\n"),
		("sp ace.txt", "s\n"),
		("café", "c\n"),
		("quo\"te", "q\n"),
	] {
		std::fs::write(repo.join(p), c).unwrap();
	}
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	// Working-tree divergences for -m/-d/-o.
	std::fs::write(repo.join("mod.txt"), "CHANGED\n").unwrap();
	std::fs::remove_file(repo.join("del.txt")).unwrap();
	std::fs::create_dir_all(repo.join("un/deep")).unwrap();
	std::fs::write(repo.join("un/u1.txt"), "u\n").unwrap();
	std::fs::write(repo.join("un/deep/u2.txt"), "u\n").unwrap();
	std::fs::write(repo.join("un/i.log"), "i\n").unwrap();
	std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();

	// At the repository root: pathspecs, selection sets, combinations, -z, quoting, --full-name.
	let root_cases: &[&[&str]] = &[
		&[],
		&["src/*.rs"],
		&["src"],
		&["*.rs"],
		&[".", ":!vendor"],
		&["nonexistent"],
		&[":(icase)CAFÉ"],
		&["-c"],
		&["-s"],
		&["-o"],
		&["-o", "--exclude-standard"],
		&["-m"],
		&["-d"],
		&["-c", "-o", "-m", "-d"],
		&["-s", "-d", "-m"],
		&["-z"],
		&["-s", "-z"],
		&["-o", "-z"],
		&["--full-name"],
		&["-o", "un/*"],
		&["--error-unmatch", "nonexistent"],
		&["--error-unmatch", "README.md"],
		&["-o", "--error-unmatch", "README.md"],
		&["-m", "--error-unmatch", "src/lib.rs"],
		&["-m", "--error-unmatch", "mod.txt"],
	];
	for case in root_cases {
		assert_same(&repo, "", case);
	}

	// In the `src/` subdirectory: cwd-relative output and pathspecs, and `--full-name` opting back to
	// repository-relative.
	let sub_cases: &[&[&str]] = &[
		&[],
		&["*.rs"],
		&["--full-name", "*.rs"],
		&["../vendor"],
		&["--full-name", "../vendor"],
		&["-o"],
	];
	for case in sub_cases {
		assert_same(&repo, "src", case);
	}

	std::fs::remove_dir_all(&repo).ok();
}

/// An unmerged (conflicted) index: `ls-files` lists a conflicted path once per stage under `-c`/`-s`,
/// once per differing stage under `-m`, and never under `-d` (the working file is present).
#[test]
fn ls_files_unmerged_matches_git() {
	let repo = git_repo("lsf-unmerged");
	std::fs::write(repo.join("f.txt"), "base\n").unwrap();
	std::fs::write(repo.join("normal.txt"), "n\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "base"]);
	git_ok(&repo, &["checkout", "-qb", "other"]);
	std::fs::write(repo.join("f.txt"), "other\n").unwrap();
	git_ok(&repo, &["commit", "-qam", "other"]);
	git_ok(&repo, &["checkout", "-q", "main"]);
	std::fs::write(repo.join("f.txt"), "main\n").unwrap();
	git_ok(&repo, &["commit", "-qam", "main"]);
	// Provoke the conflict (merge exits non-zero; ignore).
	let _ = git_raw(&repo, &["merge", "other"]);

	for case in [
		&[][..],
		&["-c"][..],
		&["-s"][..],
		&["-m"][..],
		&["-d"][..],
		&["f.txt"][..],
	] {
		assert_same(&repo, "", case);
	}

	std::fs::remove_dir_all(&repo).ok();
}

/// A sparse (skip-worktree) index: `-c`/`-s` list the omitted entry, but `-d` does not flag its absent
/// file as deleted (git ignores the working tree for a skip-worktree path).
#[test]
fn ls_files_sparse_matches_git() {
	let repo = git_repo("lsf-sparse");
	std::fs::write(repo.join("keep.txt"), "k\n").unwrap();
	std::fs::write(repo.join("sparse.txt"), "s\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["sparse-checkout", "init", "--no-cone"]);
	git_ok(&repo, &["sparse-checkout", "set", "keep.txt"]);

	for case in [&["-c"][..], &["-s"][..], &["-d"][..], &["-m"][..]] {
		assert_same(&repo, "", case);
	}

	std::fs::remove_dir_all(&repo).ok();
}

/// An exclusion-only pathspec is still scoped to the current subtree: `-C sub ls-files ':!a'` lists
/// `sub/` minus `a`, cwd-relative — not the whole repository.
#[test]
fn ls_files_exclusion_only_pathspec_scoped_to_subtree() {
	let repo = git_repo("lsf-excl");
	std::fs::create_dir_all(repo.join("sub")).unwrap();
	std::fs::write(repo.join("root.txt"), "r\n").unwrap();
	std::fs::write(repo.join("sub/a.txt"), "a\n").unwrap();
	std::fs::write(repo.join("sub/b.txt"), "b\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);

	assert_same(&repo, "sub", &[":!a.txt"]);
	assert_same(&repo, "sub", &[":!nomatch"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `--assume-unchanged` and `--skip-worktree` bits: git does not re-examine the working file for
/// `-m`/`-d` on a skip-worktree entry (present or absent), and trusts assume-unchanged for content
/// (`-m` only reports it once absent).
#[test]
fn ls_files_special_index_bits_match_git() {
	let repo = git_repo("lsf-bits");
	std::fs::write(repo.join("av.txt"), "a\n").unwrap();
	std::fs::write(repo.join("sw.txt"), "a\n").unwrap();
	std::fs::write(repo.join("plain.txt"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["update-index", "--assume-unchanged", "av.txt"]);
	git_ok(&repo, &["update-index", "--skip-worktree", "sw.txt"]);
	// Edit both flagged files (present) and the plain one.
	std::fs::write(repo.join("av.txt"), "CHANGED\n").unwrap();
	std::fs::write(repo.join("sw.txt"), "CHANGED\n").unwrap();
	std::fs::write(repo.join("plain.txt"), "CHANGED\n").unwrap();
	for case in [&["-c"][..], &["-m"][..], &["-d"][..]] {
		assert_same(&repo, "", case);
	}
	// Now delete the assume-unchanged file: git reports it under both -m and -d.
	std::fs::remove_file(repo.join("av.txt")).unwrap();
	for case in [&["-m"][..], &["-d"][..]] {
		assert_same(&repo, "", case);
	}

	std::fs::remove_dir_all(&repo).ok();
}

/// `--error-unmatch` with a mix of matching and unmatched pathspecs prints the matched entries to
/// stdout and *then* exits non-zero — matching output must not be discarded.
#[test]
fn ls_files_error_unmatch_preserves_matched_output() {
	let repo = git_repo("lsf-eu");
	std::fs::write(repo.join("a.txt"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);

	assert_same(&repo, "", &["--error-unmatch", "a.txt", "missing"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `-o --exclude-standard` honours all of git's standard exclude sources: per-directory `.gitignore`,
/// `.git/info/exclude`, and the configured `core.excludesFile`.
#[test]
fn ls_files_exclude_standard_sources_match_git() {
	let repo = git_repo("lsf-xstd");
	std::fs::write(repo.join("tracked"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("by_info.txt"), "i\n").unwrap();
	std::fs::write(repo.join("by_global.txt"), "g\n").unwrap();
	std::fs::write(repo.join("by_local.log"), "l\n").unwrap();
	std::fs::write(repo.join("keep.txt"), "k\n").unwrap();
	std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
	std::fs::write(repo.join(".git/info/exclude"), "by_info.txt\n").unwrap();
	let global = repo.join("global_ignore");
	std::fs::write(&global, "by_global.txt\n").unwrap();
	git_ok(
		&repo,
		&["config", "core.excludesFile", global.to_str().unwrap()],
	);

	assert_same(&repo, "", &["-o", "--exclude-standard"]);

	// A *relative* `core.excludesFile` resolves against the worktree toplevel — even when run from a
	// subdirectory. Point it at `<root>/rel_ignore` and check from `src/`.
	std::fs::write(repo.join("rel_ignore"), "by_global.txt\n").unwrap();
	std::fs::create_dir_all(repo.join("src")).unwrap();
	std::fs::write(repo.join("src/by_global.txt"), "g\n").unwrap();
	std::fs::write(repo.join("src/keep2.txt"), "k\n").unwrap();
	git_ok(&repo, &["config", "core.excludesFile", "rel_ignore"]);
	assert_same(&repo, "src", &["-o", "--exclude-standard"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// An untracked embedded git repository is listed by `-o` as the single opaque directory entry
/// (`inner/`), not recursed into.
#[test]
fn ls_files_others_embedded_repo_is_opaque() {
	let repo = git_repo("lsf-embed");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	let inner = repo.join("inner");
	std::fs::create_dir_all(&inner).unwrap();
	git_ok(
		&inner,
		&["init", "-q", "-b", "main", inner.to_str().unwrap()],
	);
	std::fs::write(inner.join("f.txt"), "f\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// A directory whose `.git` is empty/malformed (not a valid repository) is *not* opaque — git (and
/// gta) descend and list its contents.
#[test]
fn ls_files_others_malformed_dotgit_is_not_a_repo() {
	let repo = git_repo("lsf-badgit");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	// An empty `.git` directory, and a `.git` gitfile pointing nowhere valid.
	std::fs::create_dir_all(repo.join("emptygit/.git")).unwrap();
	std::fs::write(repo.join("emptygit/f"), "f\n").unwrap();
	std::fs::create_dir_all(repo.join("gitfile")).unwrap();
	std::fs::write(repo.join("gitfile/.git"), "gitdir: /nonexistent\n").unwrap();
	std::fs::write(repo.join("gitfile/f"), "f\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `-o` lists regular files and symlinks but omits sockets / FIFOs / devices, matching git.
#[test]
fn ls_files_others_skips_special_files() {
	use std::os::unix::net::UnixListener;

	let repo = git_repo("lsf-special");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("regular"), "r\n").unwrap();
	std::os::unix::fs::symlink("regular", repo.join("link")).unwrap();
	// A unix socket — a non-regular filesystem entry git never lists.
	let _socket = UnixListener::bind(repo.join("sock")).unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `--error-unmatch` with a pathspec deduplicates the output to one line per path — collapsing the
/// per-selector duplicates (`-c -m -d` on a deleted file) and a conflicted path's stages — while
/// without it the duplication stands.
#[test]
fn ls_files_error_unmatch_deduplicates() {
	let repo = git_repo("lsf-dedup");
	std::fs::write(repo.join("del.txt"), "a\n").unwrap();
	std::fs::write(repo.join("mod.txt"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::remove_file(repo.join("del.txt")).unwrap();
	std::fs::write(repo.join("mod.txt"), "CHANGED\n").unwrap();

	// Deduplicated with a pathspec + --error-unmatch; per-selector duplication without it.
	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "del.txt"]);
	assert_same(&repo, "", &["-c", "-m", "-d", "del.txt"]);
	assert_same(&repo, "", &["-m", "-d", "--error-unmatch"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// An explicitly empty `core.quotePath` value is malformed; git aborts, and so does gta.
#[test]
fn ls_files_invalid_quotepath_aborts_like_git() {
	let repo = git_repo("lsf-badqp");
	std::fs::write(repo.join("f.txt"), "f\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["config", "core.quotePath", "bogus"]);

	let git_out = git_raw(&repo, &["ls-files"]);
	let gta_out = gta_raw(&repo, &["ls-files"]);
	assert!(
		!git_out.status.success(),
		"git aborts on bad core.quotePath"
	);
	assert!(
		!gta_out.status.success(),
		"gta must also abort on bad core.quotePath, got: {:?}",
		String::from_utf8_lossy(&gta_out.stdout)
	);

	std::fs::remove_dir_all(&repo).ok();
}

/// A tracked regular file replaced on disk by a directory is descended into by `-o` (its new
/// contents are untracked), not suppressed as if it were a gitlink.
#[test]
fn ls_files_others_file_replaced_by_directory() {
	let repo = git_repo("lsf-f2d");
	std::fs::write(repo.join("foo"), "x\n").unwrap();
	std::fs::write(repo.join("keep"), "k\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::remove_file(repo.join("foo")).unwrap();
	std::fs::create_dir(repo.join("foo")).unwrap();
	std::fs::write(repo.join("foo/bar"), "b\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// An unreadable directory or tracked file never aborts `ls-files`: git warns and skips an unreadable
/// directory, and treats an unreadable tracked file as modified — the output and exit status match.
#[test]
fn ls_files_tolerates_unreadable_entries() {
	use std::os::unix::fs::PermissionsExt;

	let repo = git_repo("lsf-unread");
	std::fs::write(repo.join("y.txt"), "a\n").unwrap();
	std::fs::write(repo.join("unread.txt"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("y.txt"), "CHANGED\n").unwrap();
	std::fs::write(repo.join("unread.txt"), "CHANGED\n").unwrap();
	std::fs::create_dir(repo.join("good")).unwrap();
	std::fs::write(repo.join("good/g"), "g\n").unwrap();
	std::fs::create_dir(repo.join("bad")).unwrap();
	std::fs::write(repo.join("bad/b"), "b\n").unwrap();
	std::fs::set_permissions(
		repo.join("unread.txt"),
		std::fs::Permissions::from_mode(0o000),
	)
	.unwrap();
	std::fs::set_permissions(repo.join("bad"), std::fs::Permissions::from_mode(0o000)).unwrap();

	// Unreadable-but-unrelated (a modified sibling / an unreadable directory) must not abort.
	assert_same(&repo, "", &["-m", "y.txt"]);
	assert_same(&repo, "", &["-d"]);
	assert_same(&repo, "", &["-o", "good"]);
	assert_same(&repo, "", &["-o"]);
	// A selected unreadable tracked file is reported modified, not an error.
	assert_same(&repo, "", &["-m", "unread.txt"]);

	// Restore permissions so cleanup can remove the tree.
	std::fs::set_permissions(
		repo.join("unread.txt"),
		std::fs::Permissions::from_mode(0o644),
	)
	.ok();
	std::fs::set_permissions(repo.join("bad"), std::fs::Permissions::from_mode(0o755)).ok();
	std::fs::remove_dir_all(&repo).ok();
}

/// `core.fileMode=false` suppresses an executable-bit-only change under `-m`, matching git (the
/// value is resolved from the effective config).
#[test]
fn ls_files_modified_honors_filemode() {
	use std::os::unix::fs::PermissionsExt;

	let repo = git_repo("lsf-fmode");
	std::fs::write(repo.join("f.sh"), "x\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["config", "core.fileMode", "false"]);
	let mut perms = std::fs::metadata(repo.join("f.sh")).unwrap().permissions();
	perms.set_mode(0o755);
	std::fs::set_permissions(repo.join("f.sh"), perms).unwrap();

	assert_same(&repo, "", &["-m"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// A configured `core.excludesFile` that names a directory is an error, aborting both tools.
#[test]
fn ls_files_excludes_file_directory_aborts() {
	let repo = git_repo("lsf-excldir");
	std::fs::write(repo.join("f.txt"), "f\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("foo.xx"), "x\n").unwrap();
	std::fs::create_dir(repo.join("adir")).unwrap();
	git_ok(&repo, &["config", "core.excludesFile", "adir"]);

	let git_out = git_raw(&repo, &["ls-files", "-o", "--exclude-standard"]);
	let gta_out = gta_raw(&repo, &["ls-files", "-o", "--exclude-standard"]);
	assert!(
		!git_out.status.success(),
		"git aborts on a directory excludesFile"
	);
	assert!(
		!gta_out.status.success(),
		"gta must also abort, got: {:?}",
		String::from_utf8_lossy(&gta_out.stdout)
	);

	std::fs::remove_dir_all(&repo).ok();
}

/// `-o --error-unmatch` with only an exclusion pathspec exits non-zero when nothing is shown, and
/// succeeds when the exclusion still leaves an untracked file to list.
#[test]
fn ls_files_error_unmatch_exclusion_only() {
	let repo = git_repo("lsf-eu-excl");
	std::fs::write(repo.join("t"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("u"), "u\n").unwrap();

	// Only `u` untracked, excluded → nothing shown → both exit non-zero.
	assert_same(&repo, "", &["-o", "--error-unmatch", ":!u"]);
	// Add another untracked file → something shown → both exit 0.
	std::fs::write(repo.join("v"), "v\n").unwrap();
	assert_same(&repo, "", &["-o", "--error-unmatch", ":!u"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `core.ignoreCase=true` case-folds `--exclude-standard` matching: a `*.LOG` pattern also excludes
/// `x.log`, matching git (the default on case-insensitive filesystems such as macOS).
#[test]
fn ls_files_exclude_standard_honors_ignorecase() {
	let repo = git_repo("lsf-icase");
	git_ok(&repo, &["config", "core.ignoreCase", "true"]);
	std::fs::write(repo.join("t"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("x.log"), "l\n").unwrap();
	std::fs::write(repo.join(".gitignore"), "*.LOG\n").unwrap();

	assert_same(&repo, "", &["-o", "--exclude-standard"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// An embedded directory whose `.git/HEAD` is not a valid ref is not a repository — git (and gta)
/// descend and list its contents rather than collapsing to `inner/`.
#[test]
fn ls_files_others_embedded_repo_requires_valid_head() {
	let repo = git_repo("lsf-badhead");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	let inner = repo.join("inner");
	std::fs::create_dir_all(&inner).unwrap();
	git_ok(
		&inner,
		&["init", "-q", "-b", "main", inner.to_str().unwrap()],
	);
	std::fs::write(inner.join("f.txt"), "f\n").unwrap();
	std::fs::write(inner.join(".git/HEAD"), "garbage\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `--error-unmatch` deduplicates only for *literal* exact-path pathspecs; `.` and globs keep the
/// per-selector duplicates. Exit stays 0 when a positive is matched-then-excluded.
#[test]
fn ls_files_error_unmatch_literal_dedup_only() {
	let repo = git_repo("lsf-litdedup");
	std::fs::write(repo.join("del"), "a\n").unwrap();
	std::fs::write(repo.join("mod"), "a\n").unwrap();
	std::fs::write(repo.join("a"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::remove_file(repo.join("del")).unwrap();
	std::fs::write(repo.join("mod"), "CHANGED\n").unwrap();

	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "del"]); // literal → dedup
	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "."]); // dot → no dedup
	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "mo*"]); // glob → no dedup
	assert_same(&repo, "", &["--error-unmatch", "a", ":!a"]); // matched-then-excluded → exit 0

	std::fs::remove_dir_all(&repo).ok();
}

/// A directory at `.git/info/exclude` is a fatal excludes source, aborting both tools.
#[test]
fn ls_files_info_exclude_directory_aborts() {
	let repo = git_repo("lsf-infodir");
	std::fs::write(repo.join("f.txt"), "f\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("foo.xx"), "x\n").unwrap();
	let info_exclude = repo.join(".git/info/exclude");
	let _ = std::fs::remove_file(&info_exclude);
	std::fs::create_dir_all(&info_exclude).unwrap();

	let git_out = git_raw(&repo, &["ls-files", "-o", "--exclude-standard"]);
	let gta_out = gta_raw(&repo, &["ls-files", "-o", "--exclude-standard"]);
	assert!(
		!git_out.status.success(),
		"git aborts on a directory info/exclude"
	);
	assert!(
		!gta_out.status.success(),
		"gta must also abort, got: {:?}",
		String::from_utf8_lossy(&gta_out.stdout)
	);

	std::fs::remove_dir_all(&repo).ok();
}

/// Under `core.ignoreCase`, a working-tree entry that differs only in case from a tracked index path
/// is treated as that tracked path — not listed by `-o` — matching git.
#[test]
fn ls_files_others_case_folds_tracked_membership() {
	// `core.ignoreCase=true`: disk `foo` is the tracked `Foo`, not untracked.
	let folded = git_repo("lsf-icase-track");
	git_ok(&folded, &["config", "core.ignoreCase", "true"]);
	std::fs::write(folded.join("Foo"), "x\n").unwrap();
	git_ok(&folded, &["add", "Foo"]);
	git_ok(&folded, &["commit", "-qm", "init"]);
	std::fs::remove_file(folded.join("Foo")).unwrap();
	std::fs::write(folded.join("foo"), "x\n").unwrap();
	std::fs::write(folded.join("bar"), "b\n").unwrap(); // a genuinely-untracked file still lists
	assert_same(&folded, "", &["-o"]);
	std::fs::remove_dir_all(&folded).ok();

	// `core.ignoreCase=false`: disk `baz` is a distinct untracked file.
	let exact = git_repo("lsf-icase-off");
	git_ok(&exact, &["config", "core.ignoreCase", "false"]);
	std::fs::write(exact.join("Baz"), "x\n").unwrap();
	git_ok(&exact, &["add", "Baz"]);
	git_ok(&exact, &["commit", "-qm", "init"]);
	std::fs::remove_file(exact.join("Baz")).unwrap();
	std::fs::write(exact.join("baz"), "x\n").unwrap();
	assert_same(&exact, "", &["-o"]);
	std::fs::remove_dir_all(&exact).ok();
}

/// An embedded directory whose `.git/HEAD` is a syntactically-invalid symbolic ref (not a `refs/…`
/// name) is not a repository — git descends and lists its contents.
#[test]
fn ls_files_others_embedded_repo_rejects_bad_symref_head() {
	let repo = git_repo("lsf-symref-head");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	let inner = repo.join("inner");
	std::fs::create_dir_all(&inner).unwrap();
	git_ok(
		&inner,
		&["init", "-q", "-b", "main", inner.to_str().unwrap()],
	);
	std::fs::write(inner.join("f.txt"), "f\n").unwrap();
	std::fs::write(inner.join(".git/HEAD"), "ref: nonsense\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// `--error-unmatch` deduplicates only pathspecs that name an *exact file*: a bare directory pathspec
/// keeps git's per-selector duplicates, while a literal file (plain or `:(literal)`) collapses to one.
#[test]
fn ls_files_error_unmatch_dedup_by_match_type() {
	let repo = git_repo("lsf-dedup-type");
	std::fs::create_dir_all(repo.join("dir")).unwrap();
	std::fs::write(repo.join("dir/x"), "a\n").unwrap();
	std::fs::write(repo.join("dir/y"), "a\n").unwrap();
	std::fs::write(repo.join("del"), "a\n").unwrap();
	std::fs::write(repo.join("file"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("dir/x"), "CHANGED\n").unwrap();
	std::fs::write(repo.join("dir/y"), "CHANGED\n").unwrap();
	std::fs::remove_file(repo.join("del")).unwrap();
	std::fs::write(repo.join("file"), "CHANGED\n").unwrap();

	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "dir"]); // directory → no dedup
	assert_same(&repo, "", &["-c", "-m", "-d", "--error-unmatch", "del"]); // exact file → dedup
	assert_same(
		&repo,
		"",
		&["-c", "-m", "-d", "--error-unmatch", ":(literal)file"],
	); // magic literal → dedup

	std::fs::remove_dir_all(&repo).ok();
}

/// With `core.symlinks=false`, a `120000` symlink materialised as a plain-file placeholder is a
/// modification under `-m` only when its content no longer hashes to the recorded link target.
#[test]
fn ls_files_modified_symlink_placeholder() {
	let repo = git_repo("lsf-symplace");
	std::os::unix::fs::symlink("target", repo.join("lnk")).unwrap();
	std::fs::write(repo.join("target"), "x\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["config", "core.symlinks", "false"]);
	// Replace the symlink with a matching placeholder file (what a core.symlinks=false checkout writes).
	std::fs::remove_file(repo.join("lnk")).unwrap();
	std::fs::write(repo.join("lnk"), "target").unwrap();
	assert_same(&repo, "", &["-m"]);
	// A placeholder whose content diverges from the target is modified.
	std::fs::write(repo.join("lnk"), "WRONG").unwrap();
	assert_same(&repo, "", &["-m"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// A tracked submodule (gitlink) is reported by `-m` only when its checked-out `HEAD` differs from the
/// recorded commit — a clean submodule is never listed, matching git.
#[test]
fn ls_files_modified_submodule() {
	let repo = git_repo("lsf-submod");
	// A submodule source with two commits so `HEAD` can be moved.
	let source = git_repo("lsf-submod-src");
	std::fs::write(source.join("s"), "1\n").unwrap();
	git_ok(&source, &["add", "-A"]);
	git_ok(&source, &["commit", "-qm", "s1"]);
	std::fs::write(source.join("s"), "2\n").unwrap();
	git_ok(&source, &["commit", "-qam", "s2"]);

	git_ok(
		&repo,
		&[
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			source.to_str().unwrap(),
			"mod",
		],
	);
	git_ok(&repo, &["commit", "-qm", "add submodule"]);

	assert_same(&repo, "", &["-m"]); // clean → not listed
	assert_same(&repo, "", &["-c"]); // the gitlink is still a cached entry
	// Move the submodule's HEAD back a commit → now modified.
	git_ok(&repo.join("mod"), &["checkout", "-q", "HEAD~1"]);
	assert_same(&repo, "", &["-m"]);

	std::fs::remove_dir_all(&repo).ok();
	std::fs::remove_dir_all(&source).ok();
}

/// A worktree-local `config.worktree` value (under `extensions.worktreeConfig`) is honoured — e.g.
/// `core.quotePath=false` set per-worktree unquotes a non-ASCII name, matching git.
#[test]
fn ls_files_honors_worktree_config() {
	let repo = git_repo("lsf-wtcfg");
	std::fs::write(repo.join("café"), "c\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	git_ok(&repo, &["config", "extensions.worktreeConfig", "true"]);
	git_ok(&repo, &["config", "--worktree", "core.quotePath", "false"]);

	assert_same(&repo, "", &[]);

	std::fs::remove_dir_all(&repo).ok();
}

/// From a subdirectory, an exact-file pathspec (`f`, or `./f`) still deduplicates under
/// `--error-unmatch` — the spec is normalised against the invocation prefix before the comparison.
#[test]
fn ls_files_error_unmatch_dedup_in_subdirectory() {
	let repo = git_repo("lsf-subdedup");
	std::fs::create_dir_all(repo.join("sub")).unwrap();
	std::fs::write(repo.join("sub/f"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("sub/f"), "CHANGED\n").unwrap();

	assert_same(&repo, "sub", &["-c", "-m", "--error-unmatch", "f"]);
	assert_same(&repo, "sub", &["-c", "-m", "--error-unmatch", "./f"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// A submodule gitlink replaced on disk by a plain file is reported modified by `-m`, matching git.
#[test]
fn ls_files_modified_gitlink_replaced_by_file() {
	let repo = git_repo("lsf-gitlink-file");
	let source = git_repo("lsf-gitlink-file-src");
	std::fs::write(source.join("s"), "1\n").unwrap();
	git_ok(&source, &["add", "-A"]);
	git_ok(&source, &["commit", "-qm", "s1"]);

	git_ok(
		&repo,
		&[
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			source.to_str().unwrap(),
			"mod",
		],
	);
	git_ok(&repo, &["commit", "-qm", "add submodule"]);
	// Replace the submodule checkout with a plain file.
	std::fs::remove_dir_all(repo.join("mod")).unwrap();
	std::fs::write(repo.join("mod"), "x").unwrap();

	assert_same(&repo, "", &["-m"]);

	std::fs::remove_dir_all(&repo).ok();
	std::fs::remove_dir_all(&source).ok();
}

/// A nested `.git/HEAD` with leading whitespace before an otherwise-valid ref is not a repository —
/// git rejects it and descends, listing the directory's files.
#[test]
fn ls_files_others_embedded_repo_rejects_leading_whitespace_head() {
	let repo = git_repo("lsf-ws-head");
	std::fs::write(repo.join("top"), "t\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	let inner = repo.join("inner");
	std::fs::create_dir_all(&inner).unwrap();
	git_ok(
		&inner,
		&["init", "-q", "-b", "main", inner.to_str().unwrap()],
	);
	std::fs::write(inner.join("u"), "u\n").unwrap();
	std::fs::write(inner.join(".git/HEAD"), "   ref: refs/heads/main\n").unwrap();

	assert_same(&repo, "", &["-o"]);

	std::fs::remove_dir_all(&repo).ok();
}

/// An exact top-magic pathspec (`:/a`, `:(top)a`) deduplicates under `--error-unmatch` like a plain
/// literal, matching git.
#[test]
fn ls_files_error_unmatch_dedup_top_magic() {
	let repo = git_repo("lsf-topdedup");
	std::fs::write(repo.join("a"), "a\n").unwrap();
	git_ok(&repo, &["add", "-A"]);
	git_ok(&repo, &["commit", "-qm", "init"]);
	std::fs::write(repo.join("a"), "CHANGED\n").unwrap();

	assert_same(&repo, "", &["-c", "-m", "--error-unmatch", ":/a"]);
	assert_same(&repo, "", &["-c", "-m", "--error-unmatch", ":(top)a"]);

	std::fs::remove_dir_all(&repo).ok();
}
