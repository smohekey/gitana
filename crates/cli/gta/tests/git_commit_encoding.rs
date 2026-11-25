//! Commits whose text is in another encoding: `gta log` and `gta show` read them as git shows them,
//! `gta cherry-pick`, `gta revert` and `gta rebase` write what git writes from them, and warn as git
//! does when the text they carry couldn't be decoded.
#![cfg(unix)]

use std::{
	ffi::OsStr,
	os::unix::ffi::OsStrExt,
	path::{Path, PathBuf},
	process::{Command, Output, Stdio},
};

/// git's warning when a commit it writes carries text it couldn't decode.
const WARNING: &str = "Warning: commit message did not conform to UTF-8.\nYou may want to amend it \
                       after fixing the message, or set the config\nvariable i18n.commitEncoding \
                       to the encoding your project uses.\n";

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-enc-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// `git` in `dir`, with no global or system config and no automatic maintenance.
fn git_command(dir: &Path) -> Command {
	let mut command = Command::new("git");
	command
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
		.arg("-C")
		.arg(dir);
	command
}

fn ran(command: &mut Command) -> Output {
	let out = command.output().expect("run");
	assert!(
		out.status.success(),
		"{command:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	out
}

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
	ran(git_command(dir).args(args)).stdout
}

fn git_text(dir: &Path, args: &[&str]) -> String {
	String::from_utf8(git(dir, args)).unwrap().trim().to_owned()
}

/// `gta` in `dir`, which must succeed: its output and its errors.
fn gta(dir: &Path, args: &[&str]) -> (String, String) {
	let out = ran(
		Command::new(assert_cmd::cargo::cargo_bin("gta"))
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.arg("-C")
			.arg(dir)
			.args(args),
	);
	(
		String::from_utf8(out.stdout).unwrap(),
		String::from_utf8(out.stderr).unwrap(),
	)
}

/// A repository with a first commit of `base.txt`, its committer `T <t@e>`.
fn repository(tag: &str) -> PathBuf {
	let dir = unique_tmp(tag);
	git(&dir, &["init", "-q", "-b", "main"]);
	git(&dir, &["config", "user.name", "T"]);
	git(&dir, &["config", "user.email", "t@e"]);
	std::fs::write(dir.join("base.txt"), "base\n").unwrap();
	git(&dir, &["add", "base.txt"]);
	git(&dir, &["commit", "-q", "-m", "base"]);
	dir
}

/// A commit on the current branch adding `file`, written by git in ISO-8859-1: its author André and
/// its message "Café crème".
fn latin1_commit(dir: &Path, file: &str) -> String {
	std::fs::write(dir.join(file), "x\n").unwrap();
	git(dir, &["add", file]);
	let message = dir.join(".git/latin1-message");
	std::fs::write(&message, b"Caf\xe9 cr\xe8me\n\nbr\xfbl\xe9e\n").unwrap();
	ran(
		git_command(dir)
			.args(["-c", "i18n.commitEncoding=ISO-8859-1", "commit", "-q", "-F"])
			.arg(&message)
			.env("GIT_AUTHOR_NAME", OsStr::from_bytes(b"Andr\xe9"))
			.env("GIT_AUTHOR_EMAIL", "a@e")
			.env("GIT_AUTHOR_DATE", "@1700000000 +0000"),
	);
	git_text(dir, &["rev-parse", "HEAD"])
}

/// The current branch's last commit made again as one git can't decode: its author and message in
/// Latin-1 bytes with no `encoding` header (as only an old or foreign tool writes them).
fn undecodable_commit(dir: &Path) -> String {
	rewritten_commit(dir, None, b"Andr\xe9", b"Caf\xe9 cr\xe8me\n")
}

/// The current branch's last commit made again with `header` as its `encoding`, `name` its author's
/// name and `message` its message, written as they are (git checking nothing of them).
fn rewritten_commit(dir: &Path, header: Option<&str>, name: &[u8], message: &[u8]) -> String {
	let raw = git(dir, &["cat-file", "commit", "HEAD"]);
	let header_end = raw.windows(2).position(|pair| pair == b"\n\n").unwrap();
	let mut text = Vec::new();
	for line in raw[..header_end].split(|&byte| byte == b'\n') {
		if line.starts_with(b"author ") {
			text.extend_from_slice(b"author ");
			text.extend_from_slice(name);
			text.extend_from_slice(b" <a@e> 1700000000 +0000");
		} else if line.starts_with(b"encoding ") {
			continue;
		} else {
			text.extend_from_slice(line);
		}
		text.push(b'\n');
	}
	if let Some(header) = header {
		text.extend_from_slice(format!("encoding {header}\n").as_bytes());
	}
	text.push(b'\n');
	text.extend_from_slice(message);
	let mut child = git_command(dir)
		.args([
			"hash-object",
			"-t",
			"commit",
			"-w",
			"--literally",
			"--stdin",
		])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.unwrap();
	std::io::Write::write_all(&mut child.stdin.take().unwrap(), &text).unwrap();
	let out = child.wait_with_output().unwrap();
	assert!(out.status.success());
	let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
	git(dir, &["update-ref", "HEAD", &id]);
	id
}

/// `args` run by `program` (`git` or gta) in `dir`, whether or not it succeeds: whether it did, and
/// its errors.
fn errors_of(dir: &Path, program: &str, args: &[&str]) -> (bool, String) {
	let mut command = if program == "git" {
		git_command(dir)
	} else {
		let mut command = Command::new(assert_cmd::cargo::cargo_bin("gta"));
		command
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.arg("-C")
			.arg(dir);
		command
	};
	let out = command
		.env("GIT_EDITOR", "true")
		.args(args)
		.output()
		.unwrap();
	// git's own messages quote the commit's subject as its bytes, which needn't be UTF-8.
	(
		out.status.success(),
		String::from_utf8_lossy(&out.stderr).into_owned(),
	)
}

/// `HEAD`'s commit object but for its committer line (whose time differs between two runs).
fn head_without_committer(dir: &Path) -> Vec<u8> {
	let raw = git(dir, &["cat-file", "commit", "HEAD"]);
	let mut kept = Vec::new();
	for line in raw.split_inclusive(|&byte| byte == b'\n') {
		if !line.starts_with(b"committer ") {
			kept.extend_from_slice(line);
		}
	}
	kept
}

/// `HEAD`'s commit object but for its author and committer lines.
fn head_without_identities(dir: &Path) -> String {
	let raw = git(dir, &["cat-file", "commit", "HEAD"]);
	String::from_utf8(raw)
		.unwrap()
		.lines()
		.filter(|line| !line.starts_with("author ") && !line.starts_with("committer "))
		.map(|line| format!("{line}\n"))
		.collect()
}

/// `gta log` and `gta show` read a commit git wrote in ISO-8859-1 as git shows it: in UTF-8.
#[test]
fn log_and_show_read_a_commit_in_another_encoding_as_git_shows_it() {
	let dir = repository("log-show");
	let id = latin1_commit(&dir, "c.txt");
	let (log, _) = gta(&dir, &["log"]);
	assert_eq!(
		log.lines().next().unwrap(),
		git_text(&dir, &["log", "-1", "--format=%H %s"])
	);
	assert_eq!(
		log.lines().next().unwrap(),
		format!("{id} Caf\u{e9} cr\u{e8}me")
	);
	let (shown, _) = gta(&dir, &["show", "HEAD"]);
	assert!(shown.contains("Author: Andr\u{e9} <a@e>\n"), "{shown}");
	assert!(shown.contains("    Caf\u{e9} cr\u{e8}me\n"), "{shown}");
	assert!(shown.contains("    br\u{fb}l\u{e9}e\n"), "{shown}");
	std::fs::remove_dir_all(&dir).ok();
}

/// Cherry-picking and reverting a commit git wrote in ISO-8859-1 writes what git writes: the
/// author and message converted to UTF-8, no `encoding` header, and no warning.
#[test]
fn cherry_pick_and_revert_of_a_commit_in_another_encoding_write_what_git_writes() {
	let dir = repository("pick-revert");
	git(&dir, &["switch", "-q", "-c", "feature"]);
	let picked = latin1_commit(&dir, "c.txt");
	git(&dir, &["switch", "-q", "main"]);
	git(&dir, &["branch", "by-git"]);

	let (_, warned) = gta(&dir, &["cherry-pick", &picked]);
	assert_eq!(warned, "");
	let by_gta = head_without_committer(&dir);
	git(&dir, &["switch", "-q", "by-git"]);
	git(&dir, &["cherry-pick", &picked]);
	assert_eq!(
		String::from_utf8(by_gta).unwrap(),
		String::from_utf8(head_without_committer(&dir)).unwrap()
	);

	git(&dir, &["switch", "-q", "feature"]);
	git(&dir, &["branch", "-f", "by-git", "feature"]);
	let (_, warned) = gta(&dir, &["revert", &picked]);
	assert_eq!(warned, "");
	// A revert's author is whoever reverts, now: compared but for the identity lines.
	let by_gta = head_without_identities(&dir);
	git(&dir, &["switch", "-q", "by-git"]);
	git(&dir, &["revert", "--no-edit", &picked]);
	assert_eq!(by_gta, head_without_identities(&dir));
	assert!(
		by_gta.contains("Revert \"Caf\u{e9} cr\u{e8}me\""),
		"{by_gta}"
	);
	std::fs::remove_dir_all(&dir).ok();
}

/// A commit whose text can't be decoded is carried as git carries it (each byte that isn't UTF-8
/// read as Latin-1), and gta warns as git does: once for a cherry-pick and a revert that write a
/// commit, and not for a rebase that drops the commit as already applied.
#[test]
fn carrying_text_that_can_t_be_decoded_warns_as_git_does() {
	let dir = repository("guessed");
	git(&dir, &["switch", "-q", "-c", "feature"]);
	std::fs::write(dir.join("c.txt"), "x\n").unwrap();
	git(&dir, &["add", "c.txt"]);
	git(&dir, &["commit", "-q", "-m", "placeholder"]);
	let picked = undecodable_commit(&dir);
	git(&dir, &["switch", "-q", "main"]);
	std::fs::write(dir.join("m.txt"), "m\n").unwrap();
	git(&dir, &["add", "m.txt"]);
	git(&dir, &["commit", "-q", "-m", "M"]);
	git(&dir, &["branch", "by-git"]);

	let (_, warned) = gta(&dir, &["cherry-pick", &picked]);
	assert_eq!(warned, WARNING);
	let by_gta = head_without_committer(&dir);
	git(&dir, &["switch", "-q", "by-git"]);
	let by_git = ran(git_command(&dir).args(["cherry-pick", &picked]));
	assert_eq!(String::from_utf8(by_git.stderr).unwrap(), WARNING);
	assert_eq!(
		String::from_utf8(by_gta).unwrap(),
		String::from_utf8(head_without_committer(&dir)).unwrap()
	);

	git(&dir, &["switch", "-q", "feature"]);
	let (_, warned) = gta(&dir, &["revert", &picked]);
	assert_eq!(warned, WARNING);
	git(&dir, &["reset", "-q", "--hard", &picked]);

	// main already holds the commit's change (picked above), so the rebase drops it, writing
	// nothing: no warning, as git gives none (a stopped rebase is tested below).
	let (_, warned) = gta(&dir, &["rebase", "main"]);
	assert_eq!(warned, "");
	assert!(
		String::from_utf8(head_without_committer(&dir))
			.unwrap()
			.contains("Andr\u{e9} <a@e> 1700000000 +0000\n\nCaf\u{e9} cr\u{e8}me\n")
	);
	std::fs::remove_dir_all(&dir).ok();
}

/// A cherry-pick or a rebase that stops on a conflict warns, as git does, only when the commit is
/// written: by `--continue` or the commit that ends it, not when it stops. Run for gta and git alike in two repositories.
#[test]
fn a_stopped_pick_or_rebase_warns_when_continued_as_git_does() {
	for (operation, start, resume) in [
		(
			"pick",
			vec!["cherry-pick", "feature"],
			vec!["cherry-pick", "--continue"],
		),
		// A commit ending the pick carries the picked commit's author (its message is the one given).
		(
			"pick-commit",
			vec!["cherry-pick", "feature"],
			vec!["commit", "-m", "Resolved"],
		),
		(
			"rebase",
			vec!["rebase", "main"],
			vec!["rebase", "--continue"],
		),
	] {
		let mut warned = Vec::new();
		for program in ["gta", "git"] {
			let dir = repository(&format!("stopped-{operation}-{program}"));
			git(&dir, &["switch", "-q", "-c", "feature"]);
			std::fs::write(dir.join("base.txt"), "feature\n").unwrap();
			git(&dir, &["commit", "-q", "-am", "placeholder"]);
			undecodable_commit(&dir);
			git(&dir, &["switch", "-q", "main"]);
			std::fs::write(dir.join("base.txt"), "main\n").unwrap();
			git(&dir, &["commit", "-q", "-am", "M"]);
			if operation == "rebase" {
				git(&dir, &["switch", "-q", "feature"]);
			}
			let (started, stopped) = errors_of(&dir, program, &start);
			assert!(!started, "{operation} by {program} didn't stop: {stopped}");
			std::fs::write(dir.join("base.txt"), "resolved\n").unwrap();
			git(&dir, &["add", "base.txt"]);
			let (ended, resumed) = errors_of(&dir, program, &resume);
			assert!(ended, "{operation} by {program} didn't end: {resumed}");
			warned.push((
				stopped.contains(WARNING.trim_end()),
				resumed.contains(WARNING.trim_end()),
			));
			std::fs::remove_dir_all(&dir).ok();
		}
		assert_eq!(warned, [(false, true), (false, true)], "{operation}");
	}
}

/// Valid UTF-8 under an `encoding` nothing knows is carried as UTF-8 and, as by git, without a
/// warning: there was nothing to repair.
#[test]
fn valid_utf8_under_an_unknown_encoding_is_carried_without_a_warning() {
	let dir = repository("unknown-utf8");
	git(&dir, &["switch", "-q", "-c", "feature"]);
	std::fs::write(dir.join("c.txt"), "x\n").unwrap();
	git(&dir, &["add", "c.txt"]);
	git(&dir, &["commit", "-q", "-m", "placeholder"]);
	let picked = rewritten_commit(
		&dir,
		Some("no-such-thing"),
		"Andr\u{e9}".as_bytes(),
		"Caf\u{e9}\n".as_bytes(),
	);
	git(&dir, &["switch", "-q", "main"]);
	git(&dir, &["branch", "by-git"]);
	let (_, warned) = gta(&dir, &["cherry-pick", &picked]);
	assert_eq!(warned, "");
	let by_gta = head_without_committer(&dir);
	git(&dir, &["switch", "-q", "by-git"]);
	let by_git = ran(git_command(&dir).args(["cherry-pick", &picked]));
	assert_eq!(String::from_utf8(by_git.stderr).unwrap(), "");
	assert_eq!(
		String::from_utf8(by_gta).unwrap(),
		String::from_utf8(head_without_committer(&dir)).unwrap()
	);
	std::fs::remove_dir_all(&dir).ok();
}

/// A rebase warns once for each commit it replays whose text it repairs, as git does, and not for
/// one whose text needed nothing.
#[test]
fn a_rebase_warns_for_each_commit_it_replays_with_repaired_text() {
	let mut warnings = Vec::new();
	for program in ["gta", "git"] {
		let dir = repository(&format!("rebase-replays-{program}"));
		git(&dir, &["switch", "-q", "-c", "feature"]);
		for file in ["c.txt", "d.txt"] {
			std::fs::write(dir.join(file), "x\n").unwrap();
			git(&dir, &["add", file]);
			git(&dir, &["commit", "-q", "-m", "placeholder"]);
			undecodable_commit(&dir);
		}
		std::fs::write(dir.join("e.txt"), "x\n").unwrap();
		git(&dir, &["add", "e.txt"]);
		git(&dir, &["commit", "-q", "-m", "plain"]);
		git(&dir, &["switch", "-q", "main"]);
		std::fs::write(dir.join("m.txt"), "m\n").unwrap();
		git(&dir, &["add", "m.txt"]);
		git(&dir, &["commit", "-q", "-m", "M"]);
		git(&dir, &["switch", "-q", "feature"]);
		let (rebased, errors) = errors_of(&dir, program, &["rebase", "main"]);
		assert!(rebased, "{program}: {errors}");
		warnings.push(errors.matches(WARNING.trim_end()).count());
		std::fs::remove_dir_all(&dir).ok();
	}
	assert_eq!(warnings, [2, 2]);
}
