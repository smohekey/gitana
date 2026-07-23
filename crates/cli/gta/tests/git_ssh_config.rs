//! `gta` SSH command resolution — `core.sshCommand` / `GIT_SSH` precedence and the plink port variant.
//!
//! A fake `ssh` (logging its args, then running the remote command locally against a stock-`git` bare
//! repo) lets these assert *how* gitana invoked ssh: which command source was chosen, and which port
//! flag the variant selected.

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use support::{git, gta_ok, unique_tmp};

/// Run `gta` with `pairs` as its environment, first *clearing* any inherited SSH-override variables so
/// these tests are hermetic regardless of the runner's environment (`assert_cmd` inherits the parent
/// env, so omitting a pair does not unset an inherited value).
async fn gta_ssh(args: &[&str], pairs: &[(&str, &str)]) -> Output {
	let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
	let pairs: Vec<(String, String)> = pairs
		.iter()
		.map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
		.collect();
	tokio::task::spawn_blocking(move || {
		let mut command = assert_cmd::Command::cargo_bin("gta").unwrap();
		command.args(&args);
		for key in ["GIT_SSH_COMMAND", "GIT_SSH", "GIT_SSH_VARIANT"] {
			command.env_remove(key);
		}
		for (key, value) in &pairs {
			command.env(key, value);
		}
		command.output().expect("run gta")
	})
	.await
	.unwrap()
}

/// A fake `ssh` that appends its argument line to `$SSH_ARGLOG`, then runs the remote git command.
fn write_fake_ssh(dir: &Path) -> PathBuf {
	let script = dir.join("fake-ssh.sh");
	std::fs::write(
		&script,
		"#!/bin/sh\n\
		 printf '%s\\n' \"$*\" >> \"$SSH_ARGLOG\"\n\
		 for a in \"$@\"; do cmd=\"$a\"; done\n\
		 eval \"$cmd\"\n",
	)
	.unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
	}
	script
}

fn git_commit(dir: &Path, file: &str, content: &str, message: &str) {
	git(dir, &["config", "user.name", "Src"]);
	git(dir, &["config", "user.email", "src@example.com"]);
	std::fs::write(dir.join(file), content).unwrap();
	git(dir, &["add", file]);
	git(dir, &["commit", "-m", message]);
}

/// A bare stock-git source with one commit on `main`, plus a work clone to advance it.
fn seed(tag: &str) -> (PathBuf, PathBuf) {
	let base = unique_tmp(tag);
	let source = base.join("source.git");
	let work = base.join("work");
	assert!(
		std::process::Command::new("git")
			.args(["init", "--bare", "-b", "main", source.to_str().unwrap()])
			.output()
			.unwrap()
			.status
			.success()
	);
	let source = std::fs::canonicalize(&source).unwrap();
	assert!(
		std::process::Command::new("git")
			.args(["clone", source.to_str().unwrap(), work.to_str().unwrap()])
			.output()
			.unwrap()
			.status
			.success()
	);
	git_commit(&work, "f.txt", "one\n", "one");
	git(&work, &["push", "origin", "main"]);
	(source, work)
}

/// A hermetic base env (no SSH-command variable — [`gta_ssh`] clears any inherited one); extra vars
/// appended. Use this for the `core.sshCommand` / `GIT_SSH` / default-command cases.
fn base_env<'a>(log: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
	let mut e = vec![
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
		("GIT_AUTHOR_NAME", "Dev"),
		("GIT_AUTHOR_EMAIL", "dev@example.com"),
		("GIT_COMMITTER_NAME", "Dev"),
		("GIT_COMMITTER_EMAIL", "dev@example.com"),
		("SSH_ARGLOG", log),
	];
	e.extend_from_slice(extra);
	e
}

/// [`base_env`] plus `GIT_SSH_COMMAND=<fake>` — the common case.
fn env<'a>(fake: &'a str, log: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
	let mut e = base_env(log, extra);
	e.push(("GIT_SSH_COMMAND", fake));
	e
}

/// Clone `source` over SSH into a fresh `client`, returning the client path. The `port` (if any) is put
/// in the URL so a later fetch resolves it.
async fn clone(source: &Path, port: Option<u16>, fake: &str, log: &str, tag: &str) -> PathBuf {
	let client = unique_tmp(tag).join("client");
	let host = match port {
		Some(p) => format!("git@localhost:{p}"),
		None => "git@localhost".to_owned(),
	};
	let url = format!("ssh://{host}{}", source.display());
	gta_ok(
		&gta_ssh(
			&["clone", &url, client.to_str().unwrap()],
			&env(fake, log, &[]),
		)
		.await,
		"clone over ssh",
	);
	client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_honors_core_ssh_command() {
	// With no `GIT_SSH_COMMAND`, gitana must fall to `core.sshCommand` from the repo config.
	let (source, work) = seed("ssh-cfg-core");
	let scripts = unique_tmp("ssh-cfg-core-scripts");
	let fake = write_fake_ssh(&scripts);
	let fake = fake.to_str().unwrap();
	let log = scripts.join("args.log");
	let log = log.to_str().unwrap();

	let client = clone(&source, None, fake, log, "ssh-cfg-core-dst").await;
	// Configure `core.sshCommand` in the client, then fetch with GIT_SSH_COMMAND cleared.
	gta_ok(
		&gta_ssh(
			&[
				"-C",
				client.to_str().unwrap(),
				"config",
				"core.sshCommand",
				fake,
			],
			&env(fake, log, &[]),
		)
		.await,
		"set core.sshCommand",
	);
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);
	let want = git(&source, &["rev-parse", "main"]);

	// Fetch env WITHOUT GIT_SSH_COMMAND — resolution must use core.sshCommand.
	gta_ok(
		&gta_ssh(
			&["-C", client.to_str().unwrap(), "fetch"],
			&base_env(log, &[]),
		)
		.await,
		"fetch via core.sshCommand",
	);
	assert_eq!(
		git(&client, &["rev-parse", "refs/remotes/origin/main"]),
		want,
		"fetch via core.sshCommand did not advance the tracking ref",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_honors_git_ssh_program() {
	// With neither GIT_SSH_COMMAND nor core.sshCommand, gitana falls to the GIT_SSH program.
	let (source, work) = seed("ssh-cfg-gitssh");
	let scripts = unique_tmp("ssh-cfg-gitssh-scripts");
	let fake = write_fake_ssh(&scripts);
	let fake = fake.to_str().unwrap();
	let log = scripts.join("args.log");
	let log = log.to_str().unwrap();

	let client = clone(&source, None, fake, log, "ssh-cfg-gitssh-dst").await;
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);
	let want = git(&source, &["rev-parse", "main"]);

	// Fetch with GIT_SSH (program) set and GIT_SSH_COMMAND cleared.
	gta_ok(
		&gta_ssh(
			&["-C", client.to_str().unwrap(), "fetch"],
			&base_env(log, &[("GIT_SSH", fake)]),
		)
		.await,
		"fetch via GIT_SSH",
	);
	assert_eq!(
		git(&client, &["rev-parse", "refs/remotes/origin/main"]),
		want,
		"fetch via GIT_SSH did not advance the tracking ref",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_uses_plink_uppercase_port_flag() {
	// GIT_SSH_VARIANT=plink switches the port flag from `-p` to `-P`. Clone from a ported URL, then fetch
	// with the plink variant, and assert the logged ssh args used `-P <port>`.
	let (source, work) = seed("ssh-cfg-plink");
	let scripts = unique_tmp("ssh-cfg-plink-scripts");
	let fake = write_fake_ssh(&scripts);
	let fake = fake.to_str().unwrap();
	let log_path = scripts.join("args.log");
	let log = log_path.to_str().unwrap();

	let client = clone(&source, Some(2222), fake, log, "ssh-cfg-plink-dst").await;
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);

	// The default (OpenSSH) clone used `-p 2222`.
	let after_clone = std::fs::read_to_string(&log_path).unwrap();
	assert!(
		after_clone.contains("-p 2222"),
		"default clone should use OpenSSH `-p`: {after_clone}",
	);

	gta_ok(
		&gta_ssh(
			&["-C", client.to_str().unwrap(), "fetch"],
			&env(fake, log, &[("GIT_SSH_VARIANT", "plink")]),
		)
		.await,
		"fetch with plink variant",
	);
	// The plink-variant fetch used `-P 2222` (uppercase).
	let full = std::fs::read_to_string(&log_path).unwrap();
	let fetch_lines = full.strip_prefix(&after_clone).unwrap_or(&full);
	assert!(
		fetch_lines.contains("-P 2222"),
		"plink fetch should use `-P`: {fetch_lines}",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_tortoiseplink_adds_batch() {
	// TortoisePlink uses `-P` and adds `-batch` so an unattended fetch never blocks on a dialog.
	let (source, work) = seed("ssh-cfg-tortoise");
	let scripts = unique_tmp("ssh-cfg-tortoise-scripts");
	let fake = write_fake_ssh(&scripts);
	let fake = fake.to_str().unwrap();
	let log_path = scripts.join("args.log");
	let log = log_path.to_str().unwrap();

	let client = clone(&source, Some(2222), fake, log, "ssh-cfg-tortoise-dst").await;
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);
	let before = std::fs::read_to_string(&log_path).unwrap();

	gta_ok(
		&gta_ssh(
			&["-C", client.to_str().unwrap(), "fetch"],
			&env(fake, log, &[("GIT_SSH_VARIANT", "tortoiseplink")]),
		)
		.await,
		"fetch with tortoiseplink variant",
	);
	let full = std::fs::read_to_string(&log_path).unwrap();
	let fetch = full.strip_prefix(&before).unwrap_or(&full);
	assert!(
		fetch.contains("-batch") && fetch.contains("-P 2222"),
		"tortoiseplink fetch should use `-batch -P`: {fetch}",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variant_applies_to_the_default_ssh_command() {
	// With no custom command, the variant override must still apply — a plink variant on the default
	// `ssh` (resolved from PATH) uses `-P`. The fake is installed as `ssh` on a prepended PATH.
	let (source, _work) = seed("ssh-cfg-default");
	let bin = unique_tmp("ssh-cfg-default-bin");
	let ssh = bin.join("ssh");
	std::fs::write(
		&ssh,
		"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SSH_ARGLOG\"\nfor a in \"$@\"; do cmd=\"$a\"; done\neval \"$cmd\"\n",
	)
	.unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
	}
	let log_path = bin.join("args.log");
	let log = log_path.to_str().unwrap();
	let path = format!(
		"{}:{}",
		bin.display(),
		std::env::var("PATH").unwrap_or_default()
	);

	// Clone (no GIT_SSH_COMMAND) with GIT_SSH_VARIANT=plink and the fake `ssh` on PATH.
	let dst = unique_tmp("ssh-cfg-default-dst").join("client");
	let url = format!("ssh://git@localhost:2222{}", source.display());
	let mut e = vec![
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
		("SSH_ARGLOG", log),
		("GIT_SSH_VARIANT", "plink"),
		("PATH", path.as_str()),
	];
	e.push(("GIT_AUTHOR_NAME", "Dev"));
	gta_ok(
		&gta_ssh(&["clone", &url, dst.to_str().unwrap()], &e).await,
		"clone via default ssh with plink variant",
	);
	let logged = std::fs::read_to_string(&log_path).unwrap();
	assert!(
		logged.contains("-P 2222"),
		"default-ssh clone with plink variant should use `-P`: {logged}",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_core_ssh_command_is_authoritative() {
	// An explicitly empty `core.sshCommand` disables ssh (git treats it as authoritative), so a fetch
	// fails rather than silently falling back to the real `ssh` and making an unexpected connection.
	let (source, _work) = seed("ssh-cfg-empty");
	let scripts = unique_tmp("ssh-cfg-empty-scripts");
	let fake = write_fake_ssh(&scripts);
	let fake = fake.to_str().unwrap();
	let log = scripts.join("args.log");
	let log = log.to_str().unwrap();

	let client = clone(&source, None, fake, log, "ssh-cfg-empty-dst").await;
	gta_ok(
		&gta_ssh(
			&[
				"-C",
				client.to_str().unwrap(),
				"config",
				"core.sshCommand",
				"",
			],
			&env(fake, log, &[]),
		)
		.await,
		"set empty core.sshCommand",
	);
	// Fetch with no command env: resolution must honor the empty core.sshCommand and fail.
	let out = gta_ssh(
		&["-C", client.to_str().unwrap(), "fetch"],
		&base_env(log, &[]),
	)
	.await;
	assert!(
		!out.status.success(),
		"an empty core.sshCommand must not fall back to real ssh",
	);
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("empty"),
		"expected an 'empty ssh command' error, got: {}",
		String::from_utf8_lossy(&out.stderr),
	);
}
