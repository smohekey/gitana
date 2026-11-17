#![cfg(unix)]

//! `gta`'s line endings against stock `git` as the oracle: `add`'s `core.safecrlf=warn` warnings
//! (before the ignored-path advisory, too), the global attributes file wherever it's found, and a
//! malformed `GIT_ATTR_NOSYSTEM`. SHA-1 is used so a stock `git` without SHA-256 can serve.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `gta -C dir <args>` in `home`'s environment, returning the raw output.
fn gta_raw(home: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
	let mut command = assert_cmd::Command::cargo_bin("gta").unwrap();
	command.args(["-C", dir.to_str().unwrap()]).args(args);
	isolate(&mut command, home, env);
	command.output().expect("run gta")
}

/// Run `git -C dir <args>` in `home`'s environment, returning the raw output.
fn git_raw(home: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
	let mut command = Command::new("git");
	command.args(["-C", dir.to_str().unwrap()]).args(args);
	for (key, value) in isolated_env(home, env) {
		command.env(key, value);
	}
	command.output().expect("run git")
}

fn isolate(command: &mut assert_cmd::Command, home: &Path, env: &[(&str, &str)]) {
	for (key, value) in isolated_env(home, env) {
		command.env(key, value);
	}
}

/// The environment both run in: a home of their own, no system config, and `env` on top.
fn isolated_env(home: &Path, env: &[(&str, &str)]) -> Vec<(String, String)> {
	let mut vars = vec![
		("HOME".to_owned(), home.to_str().unwrap().to_owned()),
		(
			"XDG_CONFIG_HOME".to_owned(),
			home.join("xdg").to_str().unwrap().to_owned(),
		),
		("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
		("GIT_ATTR_NOSYSTEM".to_owned(), "1".to_owned()),
	];
	for (key, value) in env {
		vars.retain(|(existing, _)| existing != key);
		vars.push(((*key).to_owned(), (*value).to_owned()));
	}
	vars
}

fn git_ok(home: &Path, dir: &Path, args: &[&str]) -> String {
	let out = git_raw(home, dir, args, &[]);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap()
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

/// A home, and two repositories in it laid out alike by `files`: one for git, one for gta.
fn twins(tag: &str, files: &[(&str, &[u8])]) -> (PathBuf, PathBuf, PathBuf) {
	let home = unique_tmp(tag);
	let (git_side, gta_side) = (home.join("git"), home.join("gta"));
	for side in [&git_side, &gta_side] {
		std::fs::create_dir_all(side).unwrap();
		git_ok(&home, side, &["init", "-q", "-b", "main"]);
		git_ok(&home, side, &["config", "core.autocrlf", "false"]);
		for (path, content) in files {
			std::fs::write(side.join(path), content).unwrap();
		}
	}
	(home, git_side, gta_side)
}

fn ls_files(home: &Path, side: &Path) -> String {
	git_ok(home, side, &["ls-files", "-s"])
}

fn same_as_git(git: &Output, gta: &Output, what: &str) {
	assert_eq!(
		String::from_utf8_lossy(&gta.stderr),
		String::from_utf8_lossy(&git.stderr),
		"{what}: stderr"
	);
	assert_eq!(gta.status.code(), git.status.code(), "{what}: exit");
}

/// `gta add` prints git's warning for each file whose line endings the next checkout changes.
#[test]
fn add_warns_as_git_does() {
	let (home, git_side, gta_side) = twins(
		"eol-warn",
		&[
			(".gitattributes", b"*.txt text\n*.crlf text eol=crlf\n"),
			("crlf.txt", b"one\r\ntwo\r\n"),
			("lf.crlf", b"one\ntwo\n"),
			("lf.txt", b"one\ntwo\n"),
		],
	);
	let git = git_raw(&home, &git_side, &["add", "."], &[]);
	let gta = gta_raw(&home, &gta_side, &["add", "."], &[]);
	same_as_git(&git, &gta, "add");
	assert!(!git.stderr.is_empty(), "git warned");
	assert_eq!(ls_files(&home, &gta_side), ls_files(&home, &git_side));
}

/// When some pathspecs are ignored, the warnings go where git prints them: a tracked file's before
/// the advisory, a new file's after it.
#[test]
fn add_warns_around_the_ignored_advisory_as_git_does() {
	let (home, git_side, gta_side) = twins(
		"eol-advisory",
		&[
			(".gitattributes", b"*.txt text\n"),
			(".gitignore", b"*.log\n"),
			("z.txt", b"z\n"),
			("crlf.txt", b"one\r\ntwo\r\n"),
			("x.log", b"log\n"),
		],
	);
	for side in [&git_side, &gta_side] {
		git_ok(&home, side, &["add", "z.txt"]);
		std::fs::write(side.join("z.txt"), b"z\r\n").unwrap();
	}
	let args = ["add", "z.txt", "crlf.txt", "x.log"];
	let git = git_raw(&home, &git_side, &args, &[]);
	let gta = gta_raw(&home, &gta_side, &args, &[]);
	same_as_git(&git, &gta, "add with an ignored path");
	let stderr = String::from_utf8_lossy(&git.stderr);
	let advisory = stderr.find("The following paths").unwrap();
	assert!(
		stderr.find("'z.txt'").unwrap() < advisory && advisory < stderr.find("'crlf.txt'").unwrap()
	);
	assert_eq!(ls_files(&home, &gta_side), ls_files(&home, &git_side));
}

/// The global attributes file is read where git reads it: `core.attributesFile` (absolute, or under
/// `~`), and by default `$XDG_CONFIG_HOME/git/attributes`.
#[test]
fn reads_the_global_attributes_file_as_git_does() {
	let files: &[(&str, &[u8])] = &[("crlf.txt", b"one\r\ntwo\r\n"), ("crlf.dat", b"a\r\n")];
	for (name, setting) in [
		("default", None),
		("absolute", Some("ABSOLUTE")),
		("tilde", Some("~/attrs")),
	] {
		let (home, git_side, gta_side) = twins(&format!("eol-global-{name}"), files);
		let attributes = b"*.txt text\n";
		match setting {
			None => {
				std::fs::create_dir_all(home.join("xdg/git")).unwrap();
				std::fs::write(home.join("xdg/git/attributes"), attributes).unwrap();
			}
			Some(setting) => {
				std::fs::write(home.join("attrs"), attributes).unwrap();
				let value = if setting == "ABSOLUTE" {
					home.join("attrs").to_str().unwrap().to_owned()
				} else {
					setting.to_owned()
				};
				for side in [&git_side, &gta_side] {
					git_ok(&home, side, &["config", "core.attributesFile", &value]);
				}
			}
		}
		let git = git_raw(&home, &git_side, &["add", "."], &[]);
		let gta = gta_raw(&home, &gta_side, &["add", "."], &[]);
		same_as_git(&git, &gta, name);
		assert_eq!(
			ls_files(&home, &gta_side),
			ls_files(&home, &git_side),
			"{name}"
		);
		assert_eq!(
			git_ok(&home, &gta_side, &["cat-file", "blob", ":crlf.txt"]),
			"one\ntwo\n",
			"{name}: converted"
		);
	}
}

/// A malformed `GIT_ATTR_NOSYSTEM` is refused, as git refuses it once it reads attributes (with
/// gta's own `gta:` prefix for an error, where git says `fatal:`).
#[test]
fn refuses_a_malformed_git_attr_nosystem_as_git_does() {
	let (home, git_side, gta_side) = twins("eol-nosystem", &[("a.txt", b"a\n")]);
	let env = [("GIT_ATTR_NOSYSTEM", "maybe")];
	let git = git_raw(&home, &git_side, &["add", "."], &env);
	let gta = gta_raw(&home, &gta_side, &["add", "."], &env);
	assert!(!git.status.success() && !gta.status.success());
	let git_stderr = String::from_utf8_lossy(&git.stderr);
	let gta_stderr = String::from_utf8_lossy(&gta.stderr);
	assert_eq!(
		gta_stderr.strip_prefix("gta: "),
		git_stderr.strip_prefix("fatal: ")
	);
	assert_eq!(ls_files(&home, &gta_side), "");
}

/// A clone's checkout reads the global attributes file too, as git's does.
#[test]
fn clone_reads_the_global_attributes_file_as_git_does() {
	let home = unique_tmp("eol-clone");
	let origin = home.join("origin");
	std::fs::create_dir_all(&origin).unwrap();
	git_ok(&home, &origin, &["init", "-q", "-b", "main"]);
	git_ok(&home, &origin, &["config", "core.autocrlf", "false"]);
	std::fs::write(origin.join("a.txt"), b"a\n").unwrap();
	git_ok(&home, &origin, &["add", "a.txt"]);
	git_ok(
		&home,
		&origin,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"a",
		],
	);
	std::fs::create_dir_all(home.join("xdg/git")).unwrap();
	std::fs::write(home.join("xdg/git/attributes"), b"*.txt text eol=crlf\n").unwrap();
	let origin = origin.to_str().unwrap();
	let git = git_raw(&home, &home, &["clone", "-q", origin, "by-git"], &[]);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(&home, &home, &["clone", origin, "by-gta"], &[]);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert_eq!(std::fs::read(home.join("by-git/a.txt")).unwrap(), b"a\r\n");
	assert_eq!(std::fs::read(home.join("by-gta/a.txt")).unwrap(), b"a\r\n");
}

/// A repository with one commit (`a.txt`, LF) and per-worktree config switched on, in `home`.
fn worktree_repo(home: &Path, name: &str) -> PathBuf {
	let main = home.join(name);
	std::fs::create_dir_all(&main).unwrap();
	git_ok(home, &main, &["init", "-q", "-b", "main"]);
	for (key, value) in [
		("core.autocrlf", "false"),
		("core.repositoryformatversion", "1"),
		("extensions.worktreeConfig", "true"),
	] {
		git_ok(home, &main, &["config", key, value]);
	}
	std::fs::write(main.join("a.txt"), b"a\n").unwrap();
	git_ok(home, &main, &["add", "a.txt"]);
	git_ok(
		home,
		&main,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"a",
		],
	);
	main
}

/// `worktree remove` judges the worktree it removes by that worktree's own config, not the one it
/// runs in: an LF→CRLF edit the caller's `core.autocrlf=input` would hide is still an edit there,
/// and the removal refuses, as git's does.
#[test]
fn worktree_remove_judges_the_target_by_its_own_config_as_git_does() {
	let home = unique_tmp("eol-wt-remove");
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		git_ok(
			&home,
			&main,
			&["config", "--worktree", "core.autocrlf", "input"],
		);
		let wt = home.join(format!("{name}-wt"));
		git_ok(
			&home,
			&main,
			&["worktree", "add", "-q", wt.to_str().unwrap()],
		);
		git_ok(
			&home,
			&wt,
			&["config", "--worktree", "core.autocrlf", "false"],
		);
		std::fs::write(wt.join("a.txt"), b"a\r\n").unwrap();
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"remove",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(!git.status.success(), "git refuses");
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&[
			"worktree",
			"remove",
			home.join("by-gta-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		!gta.status.success(),
		"gta refuses: {}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert_eq!(
		std::fs::read(home.join("by-gta-wt/a.txt")).unwrap(),
		b"a\r\n"
	);
}

/// `worktree add` checks the new worktree out by the caller's config, per-worktree layer included,
/// and copies that layer into the new worktree, as git does.
#[test]
fn worktree_add_checks_out_by_the_callers_config_as_git_does() {
	let home = unique_tmp("eol-wt-add");
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		git_ok(
			&home,
			&main,
			&["config", "--worktree", "core.autocrlf", "true"],
		);
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"add",
			"-q",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&["worktree", "add", home.join("by-gta-wt").to_str().unwrap()],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert_eq!(
		std::fs::read(home.join("by-git-wt/a.txt")).unwrap(),
		b"a\r\n"
	);
	assert_eq!(
		std::fs::read(home.join("by-gta-wt/a.txt")).unwrap(),
		b"a\r\n"
	); // And keeps them, as git does: its own `config.worktree` is a copy of the caller's, so a file it
	// checked out reads as unchanged once touched.
	for name in ["by-git-wt", "by-gta-wt"] {
		let wt = home.join(name);
		assert_eq!(
			git_ok(
				&home,
				&wt,
				&["config", "--worktree", "--get", "core.autocrlf"]
			)
			.trim(),
			"true",
			"{name}"
		);
		std::thread::sleep(std::time::Duration::from_millis(20));
		std::fs::write(wt.join("a.txt"), b"a\r\n").unwrap();
		assert_eq!(git_ok(&home, &wt, &["status", "--porcelain"]), "", "{name}");
	}
}

/// A merge whose conflict lands outside the sparse cone on a file gitana can't write refuses before
/// it changes anything: no file, no index entry, no `MERGE_HEAD`.
#[test]
fn a_sparse_conflict_gitana_cannot_write_changes_nothing() {
	let home = unique_tmp("eol-sparse-conflict");
	let side = home.join("repo");
	std::fs::create_dir_all(side.join("out")).unwrap();
	std::fs::create_dir_all(side.join("keep")).unwrap();
	git_ok(&home, &side, &["init", "-q", "-b", "main"]);
	git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
	let commit = |message: &str| {
		git_ok(&home, &side, &["add", "-A"]);
		git_ok(
			&home,
			&side,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				message,
			],
		);
	};
	std::fs::write(side.join(".gitattributes"), b"out/* ident\n").unwrap();
	std::fs::write(side.join("in.txt"), b"base\n").unwrap();
	std::fs::write(side.join("keep/k.txt"), b"k\n").unwrap();
	std::fs::write(side.join("out/f.txt"), b"base\n").unwrap();
	commit("base");
	git_ok(&home, &side, &["switch", "-q", "-c", "other"]);
	std::fs::write(side.join("in.txt"), b"other\n").unwrap();
	std::fs::write(side.join("out/f.txt"), b"other\n").unwrap();
	commit("other");
	git_ok(&home, &side, &["switch", "-q", "main"]);
	std::fs::write(side.join("out/f.txt"), b"main\n").unwrap();
	commit("main");
	git_ok(&home, &side, &["sparse-checkout", "set", "--cone", "keep"]);
	let index_before = git_ok(&home, &side, &["ls-files", "-s"]);
	let out = gta_raw(&home, &side, &["merge", "other"], &[]);
	assert!(!out.status.success());
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("ident"),
		"{}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(std::fs::read(side.join("in.txt")).unwrap(), b"base\n");
	assert_eq!(git_ok(&home, &side, &["ls-files", "-s"]), index_before);
	assert!(!side.join(".git/MERGE_HEAD").exists());
}

/// Every file under `dir`, by path relative to it, outside `.git`.
fn files_under(dir: &Path) -> Vec<String> {
	fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
		for entry in std::fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			if path.file_name().unwrap() == ".git" {
				continue;
			}
			if path.is_dir() {
				walk(root, &path, out);
			} else {
				out.push(
					path
						.strip_prefix(root)
						.unwrap()
						.to_str()
						.unwrap()
						.to_owned(),
				);
			}
		}
	}
	let mut out = Vec::new();
	walk(dir, dir, &mut out);
	out.sort();
	out
}

/// `worktree add` from a sparse worktree makes a sparse one with the same patterns, as git does: it
/// copies the patterns along with `config.worktree`.
#[test]
fn worktree_add_from_a_sparse_worktree_stays_sparse_as_git_does() {
	let home = unique_tmp("eol-wt-sparse");
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		for dir in ["inc", "out"] {
			std::fs::create_dir_all(main.join(dir)).unwrap();
			std::fs::write(main.join(dir).join("f.txt"), b"f\n").unwrap();
		}
		git_ok(&home, &main, &["add", "-A"]);
		git_ok(
			&home,
			&main,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				"dirs",
			],
		);
		git_ok(&home, &main, &["sparse-checkout", "set", "--cone", "inc"]);
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"add",
			"-q",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&["worktree", "add", home.join("by-gta-wt").to_str().unwrap()],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert_eq!(
		files_under(&home.join("by-gta-wt")),
		files_under(&home.join("by-git-wt"))
	);
	assert!(!home.join("by-gta-wt/out").exists());
	assert_eq!(
		git_ok(&home, &home.join("by-gta-wt"), &["sparse-checkout", "list"]),
		git_ok(&home, &home.join("by-git-wt"), &["sparse-checkout", "list"])
	);
}

/// `worktree remove --force` cleans up a worktree whose `config.worktree` is malformed, as git's
/// does: a forced removal checks no status, so it reads none of the worktree's settings.
#[test]
fn forced_worktree_remove_ignores_a_malformed_config_as_git_does() {
	let home = unique_tmp("eol-wt-force");
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		let wt = home.join(format!("{name}-wt"));
		git_ok(
			&home,
			&main,
			&["worktree", "add", "-q", wt.to_str().unwrap()],
		);
		std::fs::write(
			main.join(format!(".git/worktrees/{name}-wt/config.worktree")),
			b"[core\n",
		)
		.unwrap();
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"remove",
			"--force",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&[
			"worktree",
			"remove",
			"--force",
			home.join("by-gta-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert!(!home.join("by-gta-wt").exists());
}

/// `worktree add` copies a `config.worktree` that's a symlink to a file outside the git folder, as
/// git does.
#[test]
fn worktree_add_copies_a_symlinked_worktree_config_as_git_does() {
	let home = unique_tmp("eol-wt-symlink");
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		let outside = home.join(format!("{name}-config.worktree"));
		std::fs::write(&outside, b"[core]\n\tautocrlf = true\n").unwrap();
		std::os::unix::fs::symlink(&outside, main.join(".git/config.worktree")).unwrap();
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"add",
			"-q",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&["worktree", "add", home.join("by-gta-wt").to_str().unwrap()],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	for name in ["by-git-wt", "by-gta-wt"] {
		assert_eq!(
			git_ok(
				&home,
				&home.join(name),
				&["config", "--worktree", "--get", "core.autocrlf"]
			)
			.trim(),
			"true",
			"{name}"
		);
		assert_eq!(
			std::fs::read(home.join(name).join("a.txt")).unwrap(),
			b"a\r\n",
			"{name}"
		);
	}
}

/// git's size shortcut: a file whose size changed counts as changed without being read, even when
/// its content as stored didn't (an LF file rewritten as CRLF under `eol=crlf`). Status and
/// `ls-files -m` list it, `rm` and a switch refuse, diff shows nothing, and `add` makes it clean, in
/// gta as in git.
#[test]
fn a_size_change_counts_as_a_change_as_git_does() {
	let home = unique_tmp("eol-size");
	let commit = |side: &Path, message: &str| {
		git_ok(
			&home,
			side,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				message,
			],
		);
	};
	for name in ["by-git", "by-gta"] {
		let side = home.join(name);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
		std::fs::write(side.join(".gitattributes"), b"file.txt text eol=crlf\n").unwrap();
		std::fs::write(side.join("file.txt"), b"old\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		commit(&side, "one");
		git_ok(&home, &side, &["switch", "-q", "-c", "other"]);
		std::fs::write(side.join("file.txt"), b"other\n").unwrap();
		git_ok(&home, &side, &["add", "file.txt"]);
		commit(&side, "other");
		git_ok(&home, &side, &["switch", "-q", "main"]);
		// The index records the LF file's size; a second on, it's rewritten as CRLF.
		std::fs::write(side.join("file.txt"), b"old\n").unwrap();
		git_ok(&home, &side, &["add", "file.txt"]);
	}
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for name in ["by-git", "by-gta"] {
		std::fs::write(home.join(name).join("file.txt"), b"old\r\n").unwrap();
	}
	let (git_side, gta_side) = (home.join("by-git"), home.join("by-gta"));
	let stdout = |out: Output| String::from_utf8(out.stdout).unwrap();
	assert_eq!(
		stdout(gta_raw(&home, &gta_side, &["status"], &[])),
		git_ok(&home, &git_side, &["status", "--porcelain"]),
		"status"
	);
	assert_eq!(
		stdout(gta_raw(&home, &gta_side, &["ls-files", "-m"], &[])),
		git_ok(&home, &git_side, &["ls-files", "-m"]),
		"ls-files -m"
	);
	assert_eq!(
		stdout(gta_raw(&home, &gta_side, &["diff"], &[])),
		git_ok(&home, &git_side, &["diff"]),
		"diff"
	);
	for args in [&["rm", "file.txt"][..], &["switch", "other"][..]] {
		let git = git_raw(&home, &git_side, args, &[]);
		let gta = gta_raw(&home, &gta_side, args, &[]);
		assert!(!git.status.success(), "git refuses {args:?}");
		assert!(!gta.status.success(), "gta refuses {args:?}");
		assert_eq!(
			std::fs::read(gta_side.join("file.txt")).unwrap(),
			b"old\r\n"
		);
	}
	git_ok(&home, &git_side, &["add", "file.txt"]);
	assert!(
		gta_raw(&home, &gta_side, &["add", "file.txt"], &[])
			.status
			.success()
	);
	assert_eq!(stdout(gta_raw(&home, &gta_side, &["status"], &[])), "");
	assert_eq!(git_ok(&home, &git_side, &["status", "--porcelain"]), "");
}

/// A file written converted from the index (a restore, a sparse checkout) has its new stat recorded,
/// as git records it, so git's size rule doesn't report it changed; a restore from a tree leaves the
/// index alone, as git does, and both report the file changed.
#[test]
fn a_converted_write_records_its_stat_as_git_does() {
	let home = unique_tmp("eol-restat");
	// Each case: its name, the git commands run before `core.autocrlf` is turned on, and the steps
	// each tool then runs.
	type Commands<'a> = &'a [&'a [&'a str]];
	let cases: [(&str, Commands, Commands); 4] = [
		("restore", &[], &[&["restore", "file.txt"]]),
		(
			"restore-head",
			&[],
			&[&["restore", "--source", "HEAD", "file.txt"]],
		),
		(
			"sparse-disable",
			&[&["sparse-checkout", "set", "--cone", "sub"]],
			&[&["sparse-checkout", "disable"]],
		),
		(
			"sparse-set",
			&[&["sparse-checkout", "set", "--cone", "sub"]],
			&[&["sparse-checkout", "set", "--no-cone", "/*"]],
		),
	];
	for (name, before, steps) in cases {
		let mut status = Vec::new();
		for tool in ["git", "gta"] {
			let side = home.join(format!("{name}-{tool}"));
			std::fs::create_dir_all(side.join("sub")).unwrap();
			std::fs::create_dir_all(side.join("other")).unwrap();
			git_ok(&home, &side, &["init", "-q", "-b", "main"]);
			git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
			for path in ["file.txt", "sub/in.txt", "other/x.txt"] {
				std::fs::write(side.join(path), b"one\n").unwrap();
			}
			git_ok(&home, &side, &["add", "."]);
			git_ok(
				&home,
				&side,
				&[
					"-c",
					"user.name=T",
					"-c",
					"user.email=t@e",
					"commit",
					"-q",
					"-m",
					"one",
				],
			);
			for args in before {
				git_ok(&home, &side, args);
			}
			git_ok(&home, &side, &["config", "core.autocrlf", "true"]);
			if name.starts_with("restore") {
				std::fs::remove_file(side.join("file.txt")).unwrap();
			}
			for args in steps {
				let out = if tool == "git" {
					git_raw(&home, &side, args, &[])
				} else {
					gta_raw(&home, &side, args, &[])
				};
				assert!(
					out.status.success(),
					"{name} {tool} {args:?}: {}",
					String::from_utf8_lossy(&out.stderr)
				);
			}
			let written = if name.starts_with("restore") {
				"file.txt"
			} else {
				"other/x.txt"
			};
			assert_eq!(
				std::fs::read(side.join(written)).unwrap(),
				b"one\r\n",
				"{name} {tool}"
			);
			status.push(if tool == "git" {
				git_ok(&home, &side, &["status", "--porcelain"])
			} else {
				String::from_utf8(gta_raw(&home, &side, &["status"], &[]).stdout).unwrap()
			});
		}
		assert_eq!(status[1], status[0], "{name}");
		let expected = if name == "restore-head" {
			" M file.txt\n"
		} else {
			""
		};
		assert_eq!(status[0], expected, "{name}");
	}
}

/// `mv` refuses a bad `filter.<name>.required` before moving anything; git fails on it too, after its
/// rename. A file `mv` can't read is moved and staged, as git moves it.
#[test]
fn mv_checks_the_filter_settings_before_moving() {
	let home = unique_tmp("eol-mv-filter");
	let mut sides = Vec::new();
	for name in ["by-git", "by-gta"] {
		let side = home.join(name);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		std::fs::write(side.join(".gitattributes"), b"g.txt filter=x\n").unwrap();
		std::fs::write(side.join("f.txt"), b"one\n").unwrap();
		std::fs::write(side.join("locked.txt"), b"one\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		git_ok(
			&home,
			&side,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				"one",
			],
		);
		sides.push(side);
	}
	let (git_side, gta_side) = (&sides[0], &sides[1]);
	let bad = ["-c", "filter.x.required=oops"];
	let git = git_raw(
		&home,
		git_side,
		&[bad[0], bad[1], "mv", "f.txt", "g.txt"],
		&[],
	);
	assert!(!git.status.success(), "git fails");
	// git fails after its rename, leaving the file moved and the index naming it as before; put it
	// back, so both sides start the second part alike.
	assert!(git_side.join("g.txt").exists() && !git_side.join("f.txt").exists());
	std::fs::rename(git_side.join("g.txt"), git_side.join("f.txt")).unwrap();
	let gta = gta_raw(
		&home,
		gta_side,
		&[bad[0], bad[1], "mv", "f.txt", "g.txt"],
		&[],
	);
	assert!(!gta.status.success(), "gta fails");
	assert!(
		String::from_utf8_lossy(&gta.stderr).contains("\"oops\""),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert!(gta_side.join("f.txt").exists() && !gta_side.join("g.txt").exists());
	assert!(!gta_side.join(".git/index.lock").exists());
	assert_eq!(
		git_ok(&home, gta_side, &["ls-files"]),
		".gitattributes\nf.txt\nlocked.txt\n"
	);

	// An unreadable file: both move and stage it.
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		for side in [git_side, gta_side] {
			let path = side.join("locked.txt");
			std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
		}
		let git = git_raw(&home, git_side, &["mv", "locked.txt", "moved.txt"], &[]);
		assert!(
			git.status.success(),
			"{}",
			String::from_utf8_lossy(&git.stderr)
		);
		let gta = gta_raw(&home, gta_side, &["mv", "locked.txt", "moved.txt"], &[]);
		assert!(
			gta.status.success(),
			"{}",
			String::from_utf8_lossy(&gta.stderr)
		);
		for side in [git_side, gta_side] {
			let path = side.join("moved.txt");
			std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
		}
		assert_eq!(
			git_ok(&home, gta_side, &["ls-files", "-s"]),
			git_ok(&home, git_side, &["ls-files", "-s"])
		);
		// Compared through git's status of each side (gta's own status pairs no renames).
		assert_eq!(
			git_ok(&home, gta_side, &["status", "--porcelain"]),
			git_ok(&home, git_side, &["status", "--porcelain"])
		);
	}
}

/// A submodule at a `.gitattributes` path isn't an attributes file: git reads the index's only when it
/// holds a blob. status, diff and add work alongside it, as git's do.
#[test]
fn a_submodule_named_gitattributes_is_not_read_as_git_does() {
	let home = unique_tmp("eol-attr-submodule");
	let sub = home.join("sub");
	std::fs::create_dir_all(&sub).unwrap();
	git_ok(&home, &sub, &["init", "-q", "-b", "main"]);
	std::fs::write(sub.join("in.txt"), b"in\n").unwrap();
	git_ok(&home, &sub, &["add", "."]);
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	git_ok(&home, &sub, &[&commit[..], &["sub"]].concat());
	for name in ["by-git", "by-gta"] {
		let side = home.join(name);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		std::fs::write(side.join("a.txt"), b"one\n").unwrap();
		git_ok(&home, &side, &["add", "a.txt"]);
		git_ok(
			&home,
			&side,
			&[
				"-c",
				"protocol.file.allow=always",
				"submodule",
				"add",
				"-q",
				sub.to_str().unwrap(),
				".gitattributes",
			],
		);
		git_ok(&home, &side, &[&commit[..], &["one"]].concat());
		std::fs::write(side.join("a.txt"), b"one\ntwo\n").unwrap();
	}
	let (git_side, gta_side) = (home.join("by-git"), home.join("by-gta"));
	let stdout = |out: Output| {
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).unwrap()
	};
	assert_eq!(
		stdout(gta_raw(&home, &gta_side, &["status"], &[])),
		git_ok(&home, &git_side, &["status", "--porcelain"]),
		"status"
	);
	// gta's diff prints no `index` line (it never has); the rest is git's.
	let git_diff: String = git_ok(&home, &git_side, &["diff"])
		.lines()
		.filter(|line| !line.starts_with("index "))
		.map(|line| format!("{line}\n"))
		.collect();
	assert_eq!(
		stdout(gta_raw(&home, &gta_side, &["diff"], &[])),
		git_diff,
		"diff"
	);
	git_ok(&home, &git_side, &["add", "a.txt"]);
	stdout(gta_raw(&home, &gta_side, &["add", "a.txt"], &[]));
	assert_eq!(
		git_ok(&home, &gta_side, &["ls-files", "-s"]),
		git_ok(&home, &git_side, &["ls-files", "-s"])
	);
	// A switch reads the target tree's attributes files, the submodule's slot among them.
	for side in [&git_side, &gta_side] {
		git_ok(&home, side, &[&commit[..], &["two"]].concat());
		git_ok(&home, side, &["branch", "older", "HEAD~1"]);
	}
	git_ok(&home, &git_side, &["switch", "-q", "older"]);
	stdout(gta_raw(&home, &gta_side, &["switch", "older"], &[]));
	for side in [&git_side, &gta_side] {
		assert_eq!(std::fs::read(side.join("a.txt")).unwrap(), b"one\n");
	}
	assert_eq!(
		git_ok(&home, &gta_side, &["status", "--porcelain"]),
		git_ok(&home, &git_side, &["status", "--porcelain"])
	);
}

/// A conflicted merge that deletes `.gitattributes` goes ahead as git's does: the attributes file it
/// removes (asking here for `ident`, which gitana won't do) no longer governs the conflicts it writes.
#[test]
fn a_conflicted_merge_that_removes_gitattributes_proceeds_as_git_does() {
	let home = unique_tmp("eol-merge-attrs");
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	for name in ["by-git", "by-gta"] {
		let side = home.join(name);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		std::fs::write(side.join(".gitattributes"), b"a ident\n").unwrap();
		std::fs::write(side.join("a"), b"base\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		git_ok(&home, &side, &[&commit[..], &["base"]].concat());
		git_ok(&home, &side, &["switch", "-q", "-c", "theirs"]);
		std::fs::remove_file(side.join(".gitattributes")).unwrap();
		std::fs::write(side.join("a"), b"theirs\n").unwrap();
		git_ok(&home, &side, &["add", "-A"]);
		git_ok(&home, &side, &[&commit[..], &["theirs"]].concat());
		git_ok(&home, &side, &["switch", "-q", "main"]);
		std::fs::write(side.join("a"), b"ours\n").unwrap();
		git_ok(&home, &side, &["add", "a"]);
		git_ok(&home, &side, &[&commit[..], &["ours"]].concat());
	}
	let (git_side, gta_side) = (home.join("by-git"), home.join("by-gta"));
	let identity = ["-c", "user.name=T", "-c", "user.email=t@e"];
	let git = git_raw(
		&home,
		&git_side,
		&[&identity[..], &["merge", "theirs"]].concat(),
		&[],
	);
	assert!(!git.status.success(), "git stops at the conflict");
	let gta = gta_raw(
		&home,
		&gta_side,
		&[&identity[..], &["merge", "theirs"]].concat(),
		&[],
	);
	assert!(!gta.status.success(), "gta stops at the conflict");
	let stderr = String::from_utf8_lossy(&gta.stderr);
	assert!(!stderr.contains("ident"), "{stderr}");
	// The conflict, after its opening marker: gitana labels our side `ours` where git writes `HEAD`
	// (as it always has); the rest is git's.
	let after_marker = |side: &Path| {
		let text = std::fs::read_to_string(side.join("a")).unwrap();
		assert!(text.starts_with("<<<<<<< "), "{text}");
		text.split_once('\n').unwrap().1.to_owned()
	};
	assert_eq!(after_marker(&gta_side), after_marker(&git_side));
	assert!(!gta_side.join(".gitattributes").exists() && !git_side.join(".gitattributes").exists());
	assert_eq!(
		git_ok(&home, &gta_side, &["ls-files", "-s"]),
		git_ok(&home, &git_side, &["ls-files", "-s"])
	);
	assert_eq!(
		git_ok(&home, &gta_side, &["status", "--porcelain"]),
		git_ok(&home, &git_side, &["status", "--porcelain"])
	);
}

/// `-text=auto` unsets `text`, as git reads it (the value after a `-` is ignored): a binary file with
/// CRLF under `* text` is staged as it is, as git stages it.
#[test]
fn an_unset_attribute_with_a_value_is_unset_as_git_reads_it() {
	let home = unique_tmp("eol-unset-value");
	let content = b"one\r\n\0two\r\n";
	let mut staged = Vec::new();
	for tool in ["git", "gta"] {
		let side = home.join(tool);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
		std::fs::write(side.join(".gitattributes"), b"* text\n*.dat -text=auto\n").unwrap();
		std::fs::write(side.join("f.dat"), content).unwrap();
		let out = if tool == "git" {
			git_raw(&home, &side, &["add", "f.dat"], &[])
		} else {
			gta_raw(&home, &side, &["add", "f.dat"], &[])
		};
		assert!(
			out.status.success(),
			"{tool}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		staged.push(git_raw(&home, &side, &["cat-file", "blob", ":f.dat"], &[]).stdout);
	}
	assert_eq!(staged[1], staged[0]);
	assert_eq!(staged[0], content);
}

/// Mid-merge, with `.gitattributes` unmerged and gone from the working tree, its stage-2 (ours)
/// version governs, as git falls back to it: `*.txt -text` keeps a CRLF file as it is under
/// `core.autocrlf=true`.
#[test]
fn an_unmerged_gitattributes_reads_its_ours_version_as_git_does() {
	let home = unique_tmp("eol-stage-two");
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	let mut staged = Vec::new();
	for tool in ["git", "gta"] {
		let side = home.join(tool);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
		std::fs::write(side.join(".gitattributes"), b"*.md text\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		git_ok(&home, &side, &[&commit[..], &["base"]].concat());
		git_ok(&home, &side, &["switch", "-q", "-c", "theirs"]);
		std::fs::write(side.join(".gitattributes"), b"*.txt text\n").unwrap();
		git_ok(&home, &side, &[&commit[..], &["theirs"], &["-a"]].concat());
		git_ok(&home, &side, &["switch", "-q", "main"]);
		std::fs::write(side.join(".gitattributes"), b"*.txt -text\n").unwrap();
		git_ok(&home, &side, &[&commit[..], &["ours"], &["-a"]].concat());
		let merge = git_raw(&home, &side, &["merge", "-q", "theirs"], &[]);
		assert!(!merge.status.success(), "the merge conflicts");
		std::fs::remove_file(side.join(".gitattributes")).unwrap();
		git_ok(&home, &side, &["config", "core.autocrlf", "true"]);
		std::fs::write(side.join("f.txt"), b"a\r\nb\r\n").unwrap();
		let out = if tool == "git" {
			git_raw(&home, &side, &["add", "f.txt"], &[])
		} else {
			gta_raw(&home, &side, &["add", "f.txt"], &[])
		};
		assert!(
			out.status.success(),
			"{tool}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		staged.push(git_raw(&home, &side, &["cat-file", "blob", ":f.txt"], &[]).stdout);
	}
	assert_eq!(staged[1], staged[0]);
	assert_eq!(staged[0], b"a\r\nb\r\n");
}

/// A submodule replaced by a CRLF file under `text=auto` is staged as git stages it: the submodule's
/// commit isn't read as a blob for git's "is it CRLF in the index" check.
#[test]
fn a_file_replacing_a_submodule_is_staged_as_git_does() {
	let home = unique_tmp("eol-gitlink-file");
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	let mut staged = Vec::new();
	for tool in ["git", "gta"] {
		let side = home.join(tool);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		std::fs::write(side.join(".gitattributes"), b"* text=auto\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		git_ok(&home, &side, &[&commit[..], &["one"]].concat());
		// A gitlink naming this repository's own commit, so the object exists but isn't a blob.
		let head = git_ok(&home, &side, &["rev-parse", "HEAD"]);
		let cacheinfo = format!("160000,{},sub", head.trim());
		git_ok(
			&home,
			&side,
			&["update-index", "--add", "--cacheinfo", &cacheinfo],
		);
		git_ok(&home, &side, &[&commit[..], &["sub"]].concat());
		std::fs::write(side.join("sub"), b"a\r\nb\r\n").unwrap();
		let out = if tool == "git" {
			git_raw(&home, &side, &["add", "sub"], &[])
		} else {
			gta_raw(&home, &side, &["add", "sub"], &[])
		};
		assert!(
			out.status.success(),
			"{tool}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		staged.push(git_ok(&home, &side, &["ls-files", "-s", "sub"]));
	}
	assert_eq!(staged[1], staged[0]);
	assert!(staged[0].starts_with("100644 "), "{}", staged[0]);
}

/// `worktree add` checks the new worktree out with the config resolved for it, as git's child
/// checkout does: an `onbranch:main` include turning on `core.autocrlf` doesn't apply to a new
/// worktree on another branch.
#[test]
fn worktree_add_resolves_conditional_config_for_the_new_worktree_as_git_does() {
	let home = unique_tmp("eol-wt-onbranch");
	let include = home.join("crlf.inc");
	std::fs::write(&include, b"[core]\n\tautocrlf = true\n").unwrap();
	for name in ["by-git", "by-gta"] {
		let main = worktree_repo(&home, name);
		git_ok(
			&home,
			&main,
			&[
				"config",
				"--add",
				"includeIf.onbranch:main.path",
				include.to_str().unwrap(),
			],
		);
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"other",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&[
			"worktree",
			"add",
			"-b",
			"other",
			home.join("by-gta-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	for name in ["by-git-wt", "by-gta-wt"] {
		assert_eq!(
			std::fs::read(home.join(name).join("a.txt")).unwrap(),
			b"a\n",
			"{name}"
		);
		assert_eq!(
			git_ok(&home, &home.join(name), &["status", "--porcelain"]),
			"",
			"{name}"
		);
	}
}

/// `worktree add` keeps the command's `-c` settings for the new worktree's checkout, as git passes
/// them to its child: `-c core.autocrlf=true` writes CRLF there.
#[test]
fn worktree_add_keeps_command_settings_for_the_checkout_as_git_does() {
	let home = unique_tmp("eol-wt-command-config");
	for name in ["by-git", "by-gta"] {
		worktree_repo(&home, name);
	}
	let git = git_raw(
		&home,
		&home.join("by-git"),
		&[
			"-c",
			"core.autocrlf=true",
			"worktree",
			"add",
			"-q",
			"-b",
			"child",
			home.join("by-git-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		git.status.success(),
		"{}",
		String::from_utf8_lossy(&git.stderr)
	);
	let gta = gta_raw(
		&home,
		&home.join("by-gta"),
		&[
			"-c",
			"core.autocrlf=true",
			"worktree",
			"add",
			"-b",
			"child",
			home.join("by-gta-wt").to_str().unwrap(),
		],
		&[],
	);
	assert!(
		gta.status.success(),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	for name in ["by-git-wt", "by-gta-wt"] {
		assert_eq!(
			std::fs::read(home.join(name).join("a.txt")).unwrap(),
			b"a\r\n",
			"{name}"
		);
	}
}

/// `worktree add` reads `config.worktree` only when the repository's config turns per-worktree config
/// on, as git does: with it off, a folder at that path is ignored and the add succeeds; with it on,
/// both fail.
#[test]
fn worktree_add_reads_config_worktree_only_when_it_is_on_as_git_does() {
	let home = unique_tmp("eol-wt-config-off");
	for on in [false, true] {
		let mut succeeded = Vec::new();
		for tool in ["git", "gta"] {
			let name = format!("{tool}-{on}");
			let main = worktree_repo(&home, &name);
			if !on {
				git_ok(
					&home,
					&main,
					&["config", "--unset", "extensions.worktreeConfig"],
				);
			}
			std::fs::create_dir(main.join(".git/config.worktree")).unwrap();
			let destination = home.join(format!("{name}-wt"));
			let destination = destination.to_str().unwrap();
			let out = if tool == "git" {
				git_raw(&home, &main, &["worktree", "add", "-q", destination], &[])
			} else {
				gta_raw(&home, &main, &["worktree", "add", destination], &[])
			};
			succeeded.push(out.status.success());
		}
		assert_eq!(succeeded, vec![!on, !on], "extension on: {on} (git, gta)");
	}
}

/// An attributes file read from the index ends at its first NUL, as git reads it (one from disk loses
/// only the rest of that line): `*.md text` after it doesn't apply.
#[test]
fn an_indexed_attributes_file_ends_at_a_nul_as_git_reads_it() {
	let home = unique_tmp("eol-index-nul");
	let mut staged = Vec::new();
	for tool in ["git", "gta"] {
		let side = home.join(tool);
		std::fs::create_dir_all(&side).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
		std::fs::write(
			side.join(".gitattributes"),
			b"*.txt -text\0junk\n*.md text\n",
		)
		.unwrap();
		git_ok(&home, &side, &["add", ".gitattributes"]);
		std::fs::remove_file(side.join(".gitattributes")).unwrap();
		assert_eq!(
			git_ok(
				&home,
				&side,
				&["check-attr", "--cached", "text", "--", "b.md"]
			),
			"b.md: text: unspecified\n"
		);
		std::fs::write(side.join("b.md"), b"one\r\n").unwrap();
		let out = if tool == "git" {
			git_raw(&home, &side, &["add", "b.md"], &[])
		} else {
			gta_raw(&home, &side, &["add", "b.md"], &[])
		};
		assert!(
			out.status.success(),
			"{tool}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		staged.push(git_raw(&home, &side, &["cat-file", "blob", ":b.md"], &[]).stdout);
	}
	assert_eq!(staged[1], staged[0]);
	assert_eq!(staged[0], b"one\r\n");
}

/// A NUL ends an attributes line, as git reads it: `filter=demo` before it names the filter, so a
/// required one is refused rather than staged as it is.
#[test]
fn a_filter_before_a_nul_is_still_refused() {
	let home = unique_tmp("eol-line-nul");
	let side = home.join("repo");
	std::fs::create_dir_all(&side).unwrap();
	git_ok(&home, &side, &["init", "-q", "-b", "main"]);
	git_ok(&home, &side, &["config", "filter.demo.required", "true"]);
	std::fs::write(side.join(".gitattributes"), b"*.txt filter=demo\0\n").unwrap();
	std::fs::write(side.join("a.txt"), b"one\n").unwrap();
	assert_eq!(
		git_ok(&home, &side, &["check-attr", "filter", "--", "a.txt"]),
		"a.txt: filter: demo\n"
	);
	let gta = gta_raw(&home, &side, &["add", "a.txt"], &[]);
	assert!(!gta.status.success(), "gta refuses");
	assert!(
		String::from_utf8_lossy(&gta.stderr).contains("filter=demo"),
		"{}",
		String::from_utf8_lossy(&gta.stderr)
	);
	assert_eq!(git_ok(&home, &side, &["ls-files"]), "");
}

/// `add .` that stages a `.gitattributes` as gone or as a symlink converts the other files without
/// its old rules, as git's index has dropped them by then: an unmerged one (stage 2 only) deleted,
/// and a tracked one replaced by a symlink.
#[test]
fn add_drops_the_rules_of_a_gitattributes_it_removes_as_git_does() {
	let home = unique_tmp("eol-stale-rules");
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	for case in ["unmerged", "symlink"] {
		let mut staged = Vec::new();
		for tool in ["git", "gta"] {
			let side = home.join(format!("{case}-{tool}"));
			std::fs::create_dir_all(&side).unwrap();
			git_ok(&home, &side, &["init", "-q", "-b", "main"]);
			git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
			std::fs::write(side.join(".gitattributes"), b"*.txt -text\n").unwrap();
			std::fs::write(side.join("a.txt"), b"one\n").unwrap();
			git_ok(&home, &side, &["add", "."]);
			git_ok(&home, &side, &[&commit[..], &["base"]].concat());
			if case == "unmerged" {
				git_ok(&home, &side, &["switch", "-q", "-c", "theirs"]);
				git_ok(&home, &side, &["rm", "-q", ".gitattributes"]);
				git_ok(&home, &side, &[&commit[..], &["gone"]].concat());
				git_ok(&home, &side, &["switch", "-q", "main"]);
				std::fs::write(side.join(".gitattributes"), b"*.txt -text\n# ours\n").unwrap();
				git_ok(&home, &side, &[&commit[..], &["ours"], &["-a"]].concat());
				let merge = git_raw(&home, &side, &["merge", "-q", "theirs"], &[]);
				assert!(!merge.status.success(), "the merge conflicts");
				std::fs::remove_file(side.join(".gitattributes")).unwrap();
			} else {
				std::fs::write(side.join("elsewhere.md"), b"*.md text\n").unwrap();
				std::fs::remove_file(side.join(".gitattributes")).unwrap();
				std::os::unix::fs::symlink("elsewhere.md", side.join(".gitattributes")).unwrap();
			}
			git_ok(&home, &side, &["config", "core.autocrlf", "input"]);
			std::fs::write(side.join("a.txt"), b"one\r\ntwo\r\n").unwrap();
			let out = if tool == "git" {
				git_raw(&home, &side, &["add", "."], &[])
			} else {
				gta_raw(&home, &side, &["add", "."], &[])
			};
			assert!(
				out.status.success(),
				"{case} {tool}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			staged.push(git_raw(&home, &side, &["cat-file", "blob", ":a.txt"], &[]).stdout);
		}
		assert_eq!(staged[1], staged[0], "{case}");
		assert_eq!(staged[0], b"one\ntwo\n", "{case}");
	}
}

/// A `.gitattributes` outside the sparse patterns isn't staged by `add .`, even with its skip-worktree
/// bit cleared and the file gone, so its rules still apply, as in git.
#[test]
fn add_keeps_the_rules_of_a_sparse_excluded_gitattributes_as_git_does() {
	let home = unique_tmp("eol-sparse-attrs");
	let mut staged = Vec::new();
	for tool in ["git", "gta"] {
		let side = home.join(tool);
		std::fs::create_dir_all(side.join("in")).unwrap();
		git_ok(&home, &side, &["init", "-q", "-b", "main"]);
		git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
		std::fs::write(side.join(".gitattributes"), b"*.txt text\n").unwrap();
		std::fs::write(side.join("in/a.txt"), b"one\n").unwrap();
		git_ok(&home, &side, &["add", "."]);
		git_ok(
			&home,
			&side,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				"base",
			],
		);
		git_ok(
			&home,
			&side,
			&["sparse-checkout", "set", "--no-cone", "/in/"],
		);
		git_ok(
			&home,
			&side,
			&["update-index", "--no-skip-worktree", ".gitattributes"],
		);
		assert!(!side.join(".gitattributes").exists());
		std::fs::write(side.join("in/a.txt"), b"one\r\ntwo\r\n").unwrap();
		let out = if tool == "git" {
			git_raw(&home, &side, &["add", "."], &[])
		} else {
			gta_raw(&home, &side, &["add", "."], &[])
		};
		assert!(
			out.status.success(),
			"{tool}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		staged.push(git_raw(&home, &side, &["cat-file", "blob", ":in/a.txt"], &[]).stdout);
		assert_eq!(
			git_ok(&home, &side, &["ls-files", ".gitattributes"]),
			".gitattributes\n",
			"{tool}"
		);
	}
	assert_eq!(staged[1], staged[0]);
	assert_eq!(staged[0], b"one\ntwo\n");
}

/// A switch or a hard reset that replaces a `.gitattributes` file with a submodule writes the other
/// files without its old rules, as git has removed it first.
#[test]
fn a_gitattributes_replaced_by_a_submodule_governs_nothing_as_git_does() {
	let home = unique_tmp("eol-gitlink-replaces");
	let commit = [
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
	];
	for case in ["switch", "reset"] {
		let mut written = Vec::new();
		for tool in ["git", "gta"] {
			let side = home.join(format!("{case}-{tool}"));
			std::fs::create_dir_all(&side).unwrap();
			git_ok(&home, &side, &["init", "-q", "-b", "main"]);
			git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
			std::fs::write(side.join("f.txt"), b"one\n").unwrap();
			git_ok(&home, &side, &["add", "f.txt"]);
			git_ok(&home, &side, &[&commit[..], &["base"]].concat());
			let head = git_ok(&home, &side, &["rev-parse", "HEAD"]);
			git_ok(&home, &side, &["switch", "-q", "-c", "withlink"]);
			std::fs::write(side.join("f.txt"), b"two\n").unwrap();
			git_ok(&home, &side, &["add", "f.txt"]);
			let cacheinfo = format!("160000,{},.gitattributes", head.trim());
			git_ok(
				&home,
				&side,
				&["update-index", "--add", "--cacheinfo", &cacheinfo],
			);
			git_ok(&home, &side, &[&commit[..], &["link"]].concat());
			git_ok(&home, &side, &["checkout", "-q", "-f", "main"]);
			std::fs::write(side.join(".gitattributes"), b"*.txt text eol=crlf\n").unwrap();
			git_ok(&home, &side, &["add", ".gitattributes"]);
			git_ok(&home, &side, &[&commit[..], &["attrs"]].concat());
			std::fs::remove_file(side.join("f.txt")).unwrap();
			git_ok(&home, &side, &["checkout", "-q", "--", "f.txt"]);
			let args: &[&str] = if case == "switch" {
				&["switch", "withlink"]
			} else {
				&["reset", "--hard", "withlink"]
			};
			let out = if tool == "git" {
				git_raw(&home, &side, args, &[])
			} else {
				gta_raw(&home, &side, args, &[])
			};
			assert!(
				out.status.success(),
				"{case} {tool}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			written.push((
				std::fs::read(side.join("f.txt")).unwrap(),
				git_ok(&home, &side, &["status", "--porcelain"]),
			));
		}
		assert_eq!(written[1], written[0], "{case}");
		assert_eq!(written[0], (b"two\n".to_vec(), String::new()), "{case}");
	}
}

/// A symlinked `.gitattributes` that `add .` stages governs by its target text, as git reads it from
/// its index: an unchanged link keeps its rules, a changed one gives the new text's.
#[test]
fn a_symlinked_gitattributes_governs_by_its_link_text_as_git_does() {
	let home = unique_tmp("eol-link-text");
	for case in ["unchanged", "changed"] {
		let mut staged = Vec::new();
		for tool in ["git", "gta"] {
			let side = home.join(format!("{case}-{tool}"));
			std::fs::create_dir_all(&side).unwrap();
			git_ok(&home, &side, &["init", "-q", "-b", "main"]);
			git_ok(&home, &side, &["config", "core.autocrlf", "false"]);
			std::os::unix::fs::symlink("*.txt -text", side.join(".gitattributes")).unwrap();
			std::fs::write(side.join("a.txt"), b"one\n").unwrap();
			git_ok(&home, &side, &["add", "."]);
			git_ok(
				&home,
				&side,
				&[
					"-c",
					"user.name=T",
					"-c",
					"user.email=t@e",
					"commit",
					"-q",
					"-m",
					"base",
				],
			);
			if case == "changed" {
				std::fs::remove_file(side.join(".gitattributes")).unwrap();
				std::os::unix::fs::symlink("nothing-here", side.join(".gitattributes")).unwrap();
			}
			git_ok(&home, &side, &["config", "core.autocrlf", "input"]);
			std::fs::write(side.join("a.txt"), b"one\r\ntwo\r\n").unwrap();
			let out = if tool == "git" {
				git_raw(&home, &side, &["add", "."], &[])
			} else {
				gta_raw(&home, &side, &["add", "."], &[])
			};
			assert!(
				out.status.success(),
				"{case} {tool}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			staged.push(git_raw(&home, &side, &["cat-file", "blob", ":a.txt"], &[]).stdout);
		}
		assert_eq!(staged[1], staged[0], "{case}");
		let expected: &[u8] = if case == "unchanged" {
			b"one\r\ntwo\r\n"
		} else {
			b"one\ntwo\n"
		};
		assert_eq!(staged[0], expected, "{case}");
	}
}

/// A filter named in a symlinked `.gitattributes`' link text is refused by `add .` as by `add <file>`.
#[test]
fn add_refuses_a_filter_named_by_a_symlinked_gitattributes() {
	let home = unique_tmp("eol-link-filter");
	let side = home.join("repo");
	std::fs::create_dir_all(&side).unwrap();
	git_ok(&home, &side, &["init", "-q", "-b", "main"]);
	std::os::unix::fs::symlink("*.dat filter=swap", side.join(".gitattributes")).unwrap();
	git_ok(&home, &side, &["add", ".gitattributes"]);
	git_ok(
		&home,
		&side,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"base",
		],
	);
	git_ok(&home, &side, &["config", "filter.swap.clean", "tr a b"]);
	git_ok(&home, &side, &["config", "filter.swap.required", "true"]);
	assert_eq!(
		git_ok(&home, &side, &["check-attr", "filter", "--", "a.dat"]),
		"a.dat: filter: swap\n"
	);
	std::fs::write(side.join("a.dat"), b"aaa\n").unwrap();
	for args in [&["add", "a.dat"][..], &["add", "."][..]] {
		let out = gta_raw(&home, &side, args, &[]);
		assert!(!out.status.success(), "{args:?} refuses");
		assert!(
			String::from_utf8_lossy(&out.stderr).contains("filter=swap"),
			"{args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	}
	assert_eq!(git_ok(&home, &side, &["ls-files"]), ".gitattributes\n");
}

/// `mv` onto a path whose attributes ask for a conversion gitana won't do doesn't vouch for the file,
/// so the next `add` reads it and refuses, rather than trusting its stat.
#[test]
fn add_after_mv_onto_a_filtered_path_refuses() {
	let home = unique_tmp("eol-mv-filtered");
	let side = home.join("repo");
	std::fs::create_dir_all(&side).unwrap();
	git_ok(&home, &side, &["init", "-q", "-b", "main"]);
	std::fs::write(side.join(".gitattributes"), b"*.dat filter=swap\n").unwrap();
	std::fs::write(side.join("f.txt"), b"aaa\n").unwrap();
	git_ok(&home, &side, &["add", "."]);
	git_ok(
		&home,
		&side,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"base",
		],
	);
	git_ok(&home, &side, &["config", "filter.swap.clean", "tr a b"]);
	git_ok(&home, &side, &["config", "filter.swap.required", "true"]);
	// A second on, so the moved file's stat would be trusted if it were recorded.
	std::thread::sleep(std::time::Duration::from_millis(1100));
	let mv = gta_raw(&home, &side, &["mv", "f.txt", "f.dat"], &[]);
	assert!(
		mv.status.success(),
		"{}",
		String::from_utf8_lossy(&mv.stderr)
	);
	std::thread::sleep(std::time::Duration::from_millis(1100));
	let add = gta_raw(&home, &side, &["add", "f.dat"], &[]);
	assert!(!add.status.success(), "add refuses");
	assert!(
		String::from_utf8_lossy(&add.stderr).contains("filter=swap"),
		"{}",
		String::from_utf8_lossy(&add.stderr)
	);
}
