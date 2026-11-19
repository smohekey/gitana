//! `gta stash` against stock git: twin repositories made alike, one changed by `git stash …` and
//! one by `gta stash …`, compared by output (git's long status after apply and pop left out, as
//! gta prints none), success, the stash commits' ids, `logs/refs/stash`, the index and the files.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const WHEN: &str = "1700000000 +0000";

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-stash-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// The environment both run in: a home of their own, no system config, fixed dates, and (unless
/// `anonymous`) a fixed identity.
fn env(home: &Path, anonymous: bool) -> Vec<(&'static str, String)> {
	let mut vars = vec![
		("HOME", home.display().to_string()),
		("XDG_CONFIG_HOME", home.join("xdg").display().to_string()),
		("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
		("GIT_ATTR_NOSYSTEM", "1".to_owned()),
		("GIT_AUTHOR_DATE", WHEN.to_owned()),
		("GIT_COMMITTER_DATE", WHEN.to_owned()),
	];
	if !anonymous {
		vars.extend([
			("GIT_AUTHOR_NAME", "A".to_owned()),
			("GIT_AUTHOR_EMAIL", "a@e".to_owned()),
			("GIT_COMMITTER_NAME", "C".to_owned()),
			("GIT_COMMITTER_EMAIL", "c@e".to_owned()),
		]);
	}
	vars
}

fn run(tool: &str, dir: &Path, args: &[&str], anonymous: bool) -> Output {
	let home = dir.parent().unwrap();
	let mut command = if tool == "git" {
		let mut command = Command::new("git");
		command.args(["-C", dir.to_str().unwrap()]);
		command
	} else {
		let mut command = Command::new(env!("CARGO_BIN_EXE_gta"));
		command.args(["-C", dir.to_str().unwrap()]);
		command
	};
	command.args(args);
	for (key, value) in env(home, anonymous) {
		command.env(key, value);
	}
	if anonymous {
		for key in [
			"GIT_AUTHOR_NAME",
			"GIT_AUTHOR_EMAIL",
			"GIT_COMMITTER_NAME",
			"GIT_COMMITTER_EMAIL",
			"EMAIL",
		] {
			command.env_remove(key);
		}
	}
	command.output().expect("run")
}

fn git(dir: &Path, args: &[&str]) -> String {
	let out = run("git", dir, args, false);
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

/// Two repositories made alike by git (`init`, `a.txt`, `b.txt`, `d/c.txt`, a commit, then
/// `setup`), named `git` and `gta` under one home.
fn twins(tag: &str, setup: impl Fn(&Path)) -> (PathBuf, PathBuf, PathBuf) {
	let base = unique_tmp(tag);
	let (by_git, by_gta) = (base.join("git"), base.join("gta"));
	for dir in [&by_git, &by_gta] {
		std::fs::create_dir_all(dir).unwrap();
		git(dir, &["init", "-q", "-b", "main"]);
		git(dir, &["config", "core.autocrlf", "false"]);
		write(dir, "a.txt", b"one\n");
		write(dir, "b.txt", b"two\n");
		write(dir, "d/c.txt", b"three\n");
		git(dir, &["add", "."]);
		git(dir, &["commit", "-q", "-m", "base"]);
		setup(dir);
	}
	(base, by_git, by_gta)
}

/// git's output with its long status (after apply and pop) left out.
fn without_status(text: &[u8]) -> String {
	let text = String::from_utf8_lossy(text);
	let mut out = Vec::new();
	let mut in_status = false;
	for line in text.lines() {
		if line.starts_with("On branch ")
			|| line.starts_with("HEAD detached ")
			|| line == "Not currently on any branch."
		{
			in_status = true;
		}
		if in_status && (line.starts_with("Dropped ") || line.starts_with("The stash entry is kept")) {
			in_status = false;
		}
		if !in_status {
			out.push(line);
		}
	}
	out.join("\n")
}

/// What a stash command leaves: the stash commits, the stash reflog, the index and the files.
fn state(dir: &Path) -> Vec<String> {
	let rev = |spec: &str| {
		String::from_utf8(run("git", dir, &["rev-parse", "-q", "--verify", spec], false).stdout)
			.unwrap()
	};
	let mut files = Vec::new();
	let mut stack = vec![dir.to_path_buf()];
	while let Some(folder) = stack.pop() {
		for entry in std::fs::read_dir(&folder).unwrap() {
			let path = entry.unwrap().path();
			if path.file_name().unwrap() == ".git" {
				continue;
			}
			let rel = path.strip_prefix(dir).unwrap().display().to_string();
			if path.is_dir() {
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
		String::from_utf8_lossy(&std::fs::read(dir.join(".git/logs/refs/stash")).unwrap_or_default())
			.into_owned(),
		git(dir, &["ls-files", "-s"]),
		git(dir, &["status", "--porcelain", "-uall"]),
		files.join("\n"),
	]
}

/// Run `args` in both, comparing output and success (`stderr_like` maps git's stderr to what gta
/// prints), then the state left.
fn both(
	by_git: &Path,
	by_gta: &Path,
	args: &[&str],
	anonymous: bool,
	stderr_like: impl Fn(String) -> String,
) -> bool {
	let git_out = run("git", by_git, args, anonymous);
	let gta_out = run("gta", by_gta, args, anonymous);
	assert_eq!(
		without_status(&gta_out.stdout),
		without_status(&git_out.stdout),
		"stdout of {args:?}"
	);
	assert_eq!(
		without_status(&gta_out.stderr),
		stderr_like(without_status(&git_out.stderr)),
		"stderr of {args:?}"
	);
	assert_eq!(
		gta_out.status.success(),
		git_out.status.success(),
		"success of {args:?}"
	);
	assert_eq!(state(by_gta), state(by_git), "state after {args:?}");
	git_out.status.success()
}

fn same(text: String) -> String {
	text
}

#[test]
fn pushes_and_pops_as_git_does() {
	let (base, by_git, by_gta) = twins("push-pop", |dir| {
		write(dir, "a.txt", b"one!\n");
		write(dir, "b.txt", b"staged\n");
		git(dir, &["add", "b.txt"]);
		write(dir, "u.txt", b"untracked\n");
	});
	both(&by_git, &by_gta, &["stash"], false, same);
	both(&by_git, &by_gta, &["stash", "list"], false, same);
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	both(
		&by_git,
		&by_gta,
		&["stash", "push", "-u", "-m", "two\n  lines"],
		false,
		same,
	);
	both(
		&by_git,
		&by_gta,
		&["stash", "show", "--name-only", "--include-untracked"],
		false,
		same,
	);
	both(
		&by_git,
		&by_gta,
		&["stash", "apply", "--index"],
		false,
		same,
	);
	both(
		&by_git,
		&by_gta,
		&["stash", "push", "-q", "--", "a.txt"],
		false,
		same,
	);
	both(&by_git, &by_gta, &["stash", "list"], false, same);
	for drop in [
		&["stash", "drop", "1"][..],
		&["stash", "drop", "stash@{0}"][..],
		&["stash", "drop"][..],
	] {
		both(&by_git, &by_gta, drop, false, same);
	}
	both(&by_git, &by_gta, &["stash"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_as_git_does() {
	let (base, by_git, by_gta) = twins("refuse", |dir| write(dir, "a.txt", b"one!\n"));
	// A pathspec matching no tracked file, from the root and from a subfolder.
	both(
		&by_git,
		&by_gta,
		&["stash", "push", "--", "nope.txt"],
		false,
		same,
	);
	both(
		&by_git.join("d"),
		&by_gta.join("d"),
		&["stash", "push", "--", "nope.txt"],
		false,
		same,
	);
	// Nothing to save.
	both(
		&by_git,
		&by_gta,
		&["stash", "push", "--", "b.txt"],
		false,
		same,
	);
	// No stashes; `stash` isn't a reflog entry.
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	both(&by_git, &by_gta, &["stash", "drop", "stash"], false, same);
	// No initial commit.
	let fresh = base.join("fresh-git");
	let fresh_gta = base.join("fresh-gta");
	for dir in [&fresh, &fresh_gta] {
		std::fs::create_dir_all(dir).unwrap();
		git(dir, &["init", "-q", "-b", "main"]);
		write(dir, "f", b"x\n");
		git(dir, &["add", "f"]);
	}
	both(&fresh, &fresh_gta, &["stash"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn a_conflicted_pop_reports_and_keeps_the_stash_as_git_does() {
	let (base, by_git, by_gta) = twins("conflict", |dir| {
		write(dir, "a.txt", b"stashed\n");
		git(dir, &["stash", "-q"]);
		write(dir, "a.txt", b"upstream\n");
		git(dir, &["commit", "-q", "-am", "up"]);
	});
	assert!(!both(&by_git, &by_gta, &["stash", "pop"], false, same));
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_a_pop_over_local_changes_as_git_does() {
	let (base, by_git, by_gta) = twins("overlap", |dir| {
		write(dir, "a.txt", b"stashed\n");
		git(dir, &["stash", "-q"]);
		write(dir, "a.txt", b"dirty\n");
	});
	assert!(!both(&by_git, &by_gta, &["stash", "pop"], false, same));
	// `--index` whose staged changes don't apply: git prints its patch errors first.
	let (base2, by_git, by_gta) = twins("index-conflict", |dir| {
		write(dir, "a.txt", b"staged\n");
		git(dir, &["add", "a.txt"]);
		git(dir, &["stash", "-q"]);
		write(dir, "a.txt", b"other\n");
		git(dir, &["add", "a.txt"]);
	});
	let only_last = |text: String| {
		text
			.lines()
			.filter(|line| {
				!line.starts_with("error: patch failed") && !line.ends_with("patch does not apply")
			})
			.collect::<Vec<_>>()
			.join("\n")
	};
	assert!(!both(
		&by_git,
		&by_gta,
		&["stash", "pop", "--index"],
		false,
		only_last
	));
	std::fs::remove_dir_all(&base).ok();
	std::fs::remove_dir_all(&base2).ok();
}

#[test]
fn falls_back_to_git_stash_s_identity_as_git_does() {
	let (base, by_git, by_gta) = twins("anonymous", |dir| write(dir, "a.txt", b"one!\n"));
	both(&by_git, &by_gta, &["stash"], true, same);
	let author = git(&by_gta, &["log", "-1", "--format=%an <%ae>", "stash"]);
	assert_eq!(author.trim(), "git stash <git@stash>");
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn an_untracked_clash_is_refused_before_anything_changes() {
	let (base, by_git, by_gta) = twins("clash", |dir| {
		write(dir, "a.txt", b"one!\n");
		write(dir, "new.txt", b"u\n");
		git(dir, &["stash", "push", "-q", "-u"]);
		write(dir, "new.txt", b"other\n");
	});
	let git_out = run("git", &by_git, &["stash", "pop"], false);
	let before = state(&by_gta);
	let gta_out = run("gta", &by_gta, &["stash", "pop"], false);
	// git's lines, but nothing applied (git applies the tracked part first).
	assert_eq!(
		without_status(&gta_out.stderr),
		without_status(&git_out.stderr)
	);
	assert_eq!(
		without_status(&gta_out.stdout),
		without_status(&git_out.stdout)
	);
	assert!(!gta_out.status.success() && !git_out.status.success());
	assert_eq!(state(&by_gta), before);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn says_a_stash_of_untracked_files_is_already_up_to_date_as_git_does() {
	let (base, by_git, by_gta) = twins("up-to-date", |dir| {
		write(dir, "u.txt", b"untracked\n");
		git(dir, &["stash", "push", "-q", "-u"]);
	});
	both(&by_git, &by_gta, &["stash", "apply", "-q"], false, same);
	// The untracked file is back, so the pop's clash refuses it, after git's merge has said so.
	assert!(!both(&by_git, &by_gta, &["stash", "pop"], false, same));
	std::fs::remove_file(by_git.join("u.txt")).unwrap();
	std::fs::remove_file(by_gta.join("u.txt")).unwrap();
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_intent_to_add_and_spells_unmatched_pathspecs_as_git_does() {
	let (base, by_git, by_gta) = twins("ita-spellings", |dir| {
		write(dir, "a.txt", b"one!\n");
		write(dir, "z.txt", b"z\n");
		write(dir, "m.txt", b"m\n");
		git(dir, &["add", "-N", "z.txt", "m.txt"]);
	});
	assert!(!both(&by_git, &by_gta, &["stash"], false, same));
	assert!(!both(
		&by_git,
		&by_gta,
		&["stash", "push", "-u", "--", "a.txt"],
		false,
		same
	));
	// From a subfolder, git's spelling of a pathspec matching nothing: `..` resolved, magic kept.
	for spec in ["../nope", ":(glob)../nope*", ":/nope", "nope/"] {
		assert!(!both(
			&by_git.join("d"),
			&by_gta.join("d"),
			&["stash", "push", "--", spec],
			false,
			same
		));
	}
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn leaves_nested_repositories_out_as_git_does() {
	for alone in [false, true] {
		let (base, by_git, by_gta) = twins("nested", |dir| {
			if !alone {
				write(dir, "a.txt", b"one!\n");
				write(dir, "top.txt", b"top\n");
			}
			git(dir, &["init", "-q", "nested"]);
			write(dir, "nested/f", b"x\n");
		});
		both(&by_git, &by_gta, &["stash", "push", "-u"], false, same);
		both(&by_git, &by_gta, &["stash", "pop"], false, same);
		std::fs::remove_dir_all(&base).ok();
	}
}

#[test]
fn advises_on_pathspecs_outside_the_sparse_checkout_as_git_does() {
	let sparse = |dir: &Path| {
		write(dir, "e/f.txt", b"four\n");
		git(dir, &["add", "e/f.txt"]);
		git(dir, &["commit", "-q", "-m", "e"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "a.txt", b"one!\n");
		write(dir, "e/f.txt", b"four!\n");
	};
	// Nothing changed there: nothing to save.
	let (base, by_git, by_gta) = twins("sparse-none", sparse);
	both(&by_git, &by_gta, &["stash", "push", "--", "d"], false, same);
	std::fs::remove_dir_all(&base).ok();
	// The same stash and output as git's; git then leaves its clean-up half done, gitana finishes it.
	for args in [
		&["stash", "push", "--", "d", "e"][..],
		&["stash", "push", "-q", "--", "d/*", "a.txt"][..],
	] {
		let (base, by_git, by_gta) = twins("sparse", sparse);
		let git_out = run("git", &by_git, args, false);
		let gta_out = run("gta", &by_gta, args, false);
		assert_eq!(
			without_status(&gta_out.stdout),
			without_status(&git_out.stdout),
			"stdout of {args:?}"
		);
		assert_eq!(
			without_status(&gta_out.stderr),
			without_status(&git_out.stderr),
			"stderr of {args:?}"
		);
		assert!(
			!gta_out.status.success() && !git_out.status.success(),
			"{args:?}"
		);
		assert_eq!(
			state(&by_gta)[..5],
			state(&by_git)[..5],
			"the stash of {args:?}"
		);
		for dir in [&by_git, &by_gta] {
			git(dir, &["reset", "-q", "--hard"]);
			git(dir, &["stash", "pop", "-q"]);
		}
		assert_eq!(state(&by_gta), state(&by_git), "after popping {args:?}");
		std::fs::remove_dir_all(&base).ok();
	}
}

#[test]
fn refuses_an_unmatched_pathspec_before_an_unmerged_index_as_git_does() {
	let (base, by_git, by_gta) = twins("unmerged-order", |dir| {
		git(dir, &["switch", "-q", "-c", "side"]);
		write(dir, "a.txt", b"side\n");
		git(dir, &["commit", "-q", "-am", "side"]);
		git(dir, &["switch", "-q", "main"]);
		write(dir, "a.txt", b"main\n");
		git(dir, &["commit", "-q", "-am", "main"]);
		let _ = run("git", dir, &["merge", "-q", "side"], false);
	});
	assert!(!both(
		&by_git,
		&by_gta,
		&["stash", "push", "--", "nope"],
		false,
		same
	));
	assert!(!both(&by_git, &by_gta, &["stash"], false, same));
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn pops_replacements_and_sparse_changes_as_git_does() {
	let (base, by_git, by_gta) = twins("replace", |dir| {
		git(dir, &["rm", "-q", "b.txt"]);
		write(dir, "b.txt/child", b"child\n");
		git(dir, &["add", "b.txt/child"]);
		git(dir, &["stash", "-q"]);
	});
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
	let (base, by_git, by_gta) = twins("sparse-back", |dir| {
		write(dir, "d/c.txt", b"changed\n");
		git(dir, &["stash", "-q"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
	});
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn moves_a_file_in_the_way_aside_as_git_does() {
	let (base, by_git, by_gta) = twins("aside", |dir| {
		write(dir, "d/new", b"stashed\n");
		git(dir, &["add", "d/new"]);
		git(dir, &["stash", "-q"]);
		git(dir, &["sparse-checkout", "set", "--cone", "e"]);
		write(dir, "d/new", b"the user's\n");
	});
	// git's random backup name, the same in both once its 6 random characters are masked.
	let mask = |text: String| {
		let mut out = String::new();
		let mut rest = text.as_str();
		while let Some(at) = rest.find(".stash.") {
			let (head, tail) = rest.split_at(at + ".stash.".len());
			out.push_str(head);
			out.push_str("XXXXXX");
			rest = tail.get(6..).unwrap_or_default();
		}
		out.push_str(rest);
		out
	};
	let git_out = run("git", &by_git, &["stash", "pop"], false);
	let gta_out = run("gta", &by_gta, &["stash", "pop"], false);
	assert_eq!(
		mask(without_status(&gta_out.stdout)),
		mask(without_status(&git_out.stdout))
	);
	assert_eq!(
		without_status(&gta_out.stderr),
		without_status(&git_out.stderr)
	);
	assert!(gta_out.status.success() && git_out.status.success());
	assert_eq!(
		state(&by_gta).into_iter().map(mask).collect::<Vec<_>>(),
		state(&by_git).into_iter().map(mask).collect::<Vec<_>>()
	);
	std::fs::remove_dir_all(&base).ok();
}

/// A stash holding untracked `Foo` and `foo`, as a case-sensitive machine could make it, built
/// from objects.
fn stash_of_two_untracked_names(dir: &Path) {
	use std::io::Write;
	let with_input = |args: &[&str], input: &[u8]| {
		let mut command = Command::new("git");
		command.args(["-C", dir.to_str().unwrap()]).args(args);
		for (key, value) in env(dir.parent().unwrap(), false) {
			command.env(key, value);
		}
		let mut child = command
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.spawn()
			.unwrap();
		child.stdin.take().unwrap().write_all(input).unwrap();
		let out = child.wait_with_output().unwrap();
		assert!(out.status.success(), "git {args:?}");
		String::from_utf8(out.stdout).unwrap().trim().to_owned()
	};
	let upper = with_input(&["hash-object", "-w", "--stdin"], b"upper\n");
	let lower = with_input(&["hash-object", "-w", "--stdin"], b"lower\n");
	let u_tree = with_input(
		&["mktree"],
		format!("100644 blob {upper}\tFoo\n100644 blob {lower}\tfoo\n").as_bytes(),
	);
	let head = git(dir, &["rev-parse", "HEAD"]);
	let tree = git(dir, &["rev-parse", "HEAD^{tree}"]);
	let i = git(
		dir,
		&[
			"commit-tree",
			"-p",
			head.trim(),
			"-m",
			"index on main",
			tree.trim(),
		],
	);
	let u = git(
		dir,
		&["commit-tree", "-m", "untracked files on main", &u_tree],
	);
	let w = git(
		dir,
		&[
			"commit-tree",
			"-p",
			head.trim(),
			"-p",
			i.trim(),
			"-p",
			u.trim(),
			"-m",
			"WIP on main",
			tree.trim(),
		],
	);
	git(
		dir,
		&["update-ref", "--create-reflog", "refs/stash", w.trim()],
	);
}

#[test]
fn refuses_untracked_names_the_filesystem_sees_as_one_as_git_words_it() {
	let (base, by_git, by_gta) = twins("fold", stash_of_two_untracked_names);
	let probe = base.join("Case-Probe");
	std::fs::write(&probe, b"").unwrap();
	let folds = base.join("case-probe").exists();
	std::fs::remove_file(&probe).unwrap();
	if folds {
		// git writes `Foo` first, then fails; gitana refuses with git's words, nothing written.
		let before = state(&by_gta);
		let git_out = run("git", &by_git, &["stash", "pop"], false);
		let gta_out = run("gta", &by_gta, &["stash", "pop"], false);
		assert_eq!(
			without_status(&gta_out.stdout),
			without_status(&git_out.stdout)
		);
		assert_eq!(
			without_status(&gta_out.stderr),
			without_status(&git_out.stderr)
		);
		assert!(!gta_out.status.success() && !git_out.status.success());
		assert_eq!(state(&by_gta), before);
	} else {
		both(&by_git, &by_gta, &["stash", "pop"], false, same);
	}
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn pops_a_file_replaced_by_untracked_files_as_git_does() {
	let (base, by_git, by_gta) = twins("file-to-untracked", |dir| {
		git(dir, &["rm", "-q", "b.txt"]);
		write(dir, "b.txt/u", b"untracked\n");
	});
	both(&by_git, &by_gta, &["stash", "push", "-u"], false, same);
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn forgets_a_deleted_gitattributes_as_git_does() {
	let (base, by_git, by_gta) = twins("deleted-attributes", |dir| {
		write(dir, ".gitattributes", b"a.txt text eol=lf\n");
		git(dir, &["add", ".gitattributes"]);
		git(dir, &["commit", "-q", "-m", "attributes"]);
		std::fs::remove_file(dir.join(".gitattributes")).unwrap();
		write(dir, "a.txt", b"one\r\n");
	});
	both(&by_git, &by_gta, &["stash"], false, same);
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_to_lose_an_untracked_file_the_stash_doesnt_hold() {
	let (base, _, by_gta) = twins("unsaved", |dir| {
		std::fs::remove_file(dir.join("b.txt")).unwrap();
		write(dir, "b.txt/precious", b"precious\n");
	});
	let before = state(&by_gta);
	let out = run("gta", &by_gta, &["stash", "push", "--", "b.txt"], false);
	assert!(!out.status.success());
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("b.txt/precious"),
		"{}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(state(&by_gta), before);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn keeps_a_deleted_staged_addition_as_git_does() {
	let (base, by_git, by_gta) = twins("added-deleted", |dir| {
		write(dir, "new", b"new\n");
		git(dir, &["add", "new"]);
		std::fs::remove_file(dir.join("new")).unwrap();
	});
	both(&by_git, &by_gta, &["stash"], false, same);
	both(&by_git, &by_gta, &["stash", "pop"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

#[test]
fn leaves_a_gitfile_checkout_out_as_git_does() {
	let (base, by_git, by_gta) = twins("gitfile", |dir| {
		git(dir, &["branch", "-q", "side"]);
		git(dir, &["worktree", "add", "-q", "nested", "side"]);
		write(dir, "nested/inner.txt", b"inner\n");
		write(dir, "top.txt", b"top\n");
	});
	let git_out = run(
		"git",
		&by_git,
		&["ls-files", "-o", "--exclude-standard"],
		false,
	);
	let gta_out = run(
		"gta",
		&by_gta,
		&["ls-files", "-o", "--exclude-standard"],
		false,
	);
	assert_eq!(
		String::from_utf8_lossy(&gta_out.stdout),
		String::from_utf8_lossy(&git_out.stdout)
	);
	both(&by_git, &by_gta, &["stash", "push", "-u"], false, same);
	std::fs::remove_dir_all(&base).ok();
}

/// A drop whose log can't be rewritten is refused before anything changes, as git refuses it
/// (git's message names the lock it couldn't create, gta's the file store's error), whether the
/// stash dropped is the last or not.
#[cfg(unix)]
#[test]
fn refuses_a_drop_it_cannot_record_as_git_does() {
	use std::os::unix::fs::PermissionsExt;
	for count in [1, 2] {
		let (base, by_git, by_gta) = twins(&format!("drop-read-only-{count}"), |dir| {
			for round in 0..count {
				write(dir, "a.txt", format!("round {round}\n").as_bytes());
				git(dir, &["stash", "-q"]);
			}
		});
		let logs = |dir: &Path, mode| {
			std::fs::set_permissions(
				dir.join(".git/logs/refs"),
				std::fs::Permissions::from_mode(mode),
			)
			.unwrap();
		};
		logs(&by_git, 0o555);
		logs(&by_gta, 0o555);
		let git_out = run("git", &by_git, &["stash", "drop"], false);
		let gta_out = run("gta", &by_gta, &["stash", "drop"], false);
		logs(&by_git, 0o755);
		logs(&by_gta, 0o755);
		assert_eq!(
			gta_out.status.success(),
			git_out.status.success(),
			"{}",
			String::from_utf8_lossy(&gta_out.stderr)
		);
		assert_eq!(state(&by_gta), state(&by_git), "{count}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// A drop while the log's lock is held (another git rewriting it, or a lock left behind) is refused
/// before anything changes, as git refuses it.
#[test]
fn refuses_a_drop_while_the_log_is_locked_as_git_does() {
	let (base, by_git, by_gta) = twins("drop-log-locked", |dir| {
		write(dir, "a.txt", b"one!\n");
		git(dir, &["stash", "-q"]);
		write(dir, ".git/logs/refs/stash.lock", b"");
	});
	let git_out = run("git", &by_git, &["stash", "drop"], false);
	let gta_out = run("gta", &by_gta, &["stash", "drop"], false);
	assert!(!git_out.status.success());
	assert!(!gta_out.status.success());
	assert_eq!(state(&by_gta), state(&by_git));
	std::fs::remove_dir_all(&base).ok();
}

/// A drop whose log can be locked but not removed or rewritten (an immutable file, macOS's
/// `chflags uchg`) is refused with nothing changed, as git refuses it, whether the stash dropped is
/// the last or not.
#[cfg(target_os = "macos")]
#[test]
fn refuses_a_drop_whose_log_is_immutable_as_git_does() {
	for count in [1, 2] {
		let (base, by_git, by_gta) = twins(&format!("drop-immutable-{count}"), |dir| {
			for round in 0..count {
				write(dir, "a.txt", format!("round {round}\n").as_bytes());
				git(dir, &["stash", "-q"]);
			}
		});
		let flags = |dir: &Path, flag: &str| {
			let log = dir.join(".git/logs/refs/stash");
			let status = std::process::Command::new("chflags")
				.arg(flag)
				.arg(&log)
				.status()
				.unwrap();
			assert!(status.success());
		};
		flags(&by_git, "uchg");
		flags(&by_gta, "uchg");
		let git_out = run("git", &by_git, &["stash", "drop"], false);
		let gta_out = run("gta", &by_gta, &["stash", "drop"], false);
		flags(&by_git, "nouchg");
		flags(&by_gta, "nouchg");
		assert!(!git_out.status.success());
		assert!(
			!gta_out.status.success(),
			"{}",
			String::from_utf8_lossy(&gta_out.stdout)
		);
		assert_eq!(state(&by_gta), state(&by_git), "{count}");
		std::fs::remove_dir_all(&base).ok();
	}
}

/// Untracked files that can't be removed (a read-only folder): the stash is saved, the others
/// removed, a warning given for each left, from the top and from a folder, and the tracked files
/// left as they are, with a failure, as git does.
#[cfg(unix)]
#[test]
fn reports_untracked_files_it_cannot_remove_as_git_does() {
	use std::os::unix::fs::PermissionsExt;
	for from in ["", "d"] {
		let (base, by_git, by_gta) = twins(&format!("unremovable{}", from.len()), |dir| {
			write(dir, "a.txt", b"one!\n");
			write(dir, "locked/u.txt", b"u\n");
			write(dir, "locked/sub/v.txt", b"v\n");
			write(dir, "free/w.txt", b"w\n");
		});
		// Gone afterwards where permissions don't hold (as root, both remove everything).
		let locked = |dir: &Path, mode| {
			if dir.join("locked").exists() {
				std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(mode))
					.unwrap();
			}
		};
		locked(&by_git, 0o555);
		locked(&by_gta, 0o555);
		// From the folder itself rather than `-C`: with `-C`, git's clean-up names paths from the
		// process's own folder (from the top when that's outside the repository).
		let run_from = |tool: &str, top: &Path| {
			let program = if tool == "git" {
				std::path::PathBuf::from("git")
			} else {
				std::path::PathBuf::from(env!("CARGO_BIN_EXE_gta"))
			};
			let mut command = std::process::Command::new(program);
			command
				.current_dir(top.join(from))
				.args(["stash", "push", "-u"]);
			for (key, value) in env(top.parent().unwrap(), false) {
				command.env(key, value);
			}
			command.output().unwrap()
		};
		let git_out = run_from("git", &by_git);
		let gta_out = run_from("gta", &by_gta);
		locked(&by_git, 0o755);
		locked(&by_gta, 0o755);
		assert_eq!(
			String::from_utf8_lossy(&gta_out.stdout),
			String::from_utf8_lossy(&git_out.stdout),
			"{from:?}"
		);
		assert_eq!(
			String::from_utf8_lossy(&gta_out.stderr),
			String::from_utf8_lossy(&git_out.stderr),
			"{from:?}"
		);
		assert_eq!(
			gta_out.status.success(),
			git_out.status.success(),
			"{from:?}"
		);
		assert_eq!(state(&by_gta), state(&by_git), "{from:?}");
		std::fs::remove_dir_all(&base).ok();
	}
}
