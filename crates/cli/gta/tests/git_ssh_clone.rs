//! End-to-end `gta clone` over the SSH transport.
//!
//! gitana drives an `ssh` subprocess to run `git-upload-pack '<path>'` on the remote host. These tests
//! stand in a fake `ssh` (via `GIT_SSH_COMMAND`) that ignores the host and runs the remote command
//! locally against a **stock `git`** repository — so this is a real interop check: gitana's SSH *client*
//! negotiating with stock `git-upload-pack`, over the plain pkt-line framing SSH uses (no HTTP
//! `?service=` banner). git's own test suite fakes `ssh` the same way.

mod support;

use std::path::Path;

use support::{git, git_supports_sha256, gta_env, gta_ok, unique_tmp};

/// Write a fake `ssh` that ignores its options/host and runs the remote git command locally. gitana
/// invokes `GIT_SSH_COMMAND` as `sh -c '<cmd> "$@"' ssh [-p port] <host> "git-upload-pack '<path>'"`,
/// so the script's last argument is the remote command; `eval` runs it (`git-upload-pack` on `PATH`).
fn write_fake_ssh(dir: &Path) -> std::path::PathBuf {
	let script = dir.join("fake-ssh.sh");
	std::fs::write(
		&script,
		"#!/bin/sh\n\
		 # Ignore ssh options and the host; the last argument is the remote git command.\n\
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

/// Create a stock-`git` source repository (in `object_format`) with two commits on `main` and a
/// second branch, returning its absolute path — the path a `git-upload-pack '<path>'` serves.
fn make_source(object_format: &str) -> std::path::PathBuf {
	let work = unique_tmp(&format!("ssh-src-{object_format}"));
	let dir = work.to_str().unwrap();
	assert!(
		std::process::Command::new("git")
			.args([
				"init",
				&format!("--object-format={object_format}"),
				"-b",
				"main",
				dir
			])
			.output()
			.expect("git init")
			.status
			.success()
	);
	git(&work, &["config", "user.name", "Src Author"]);
	git(&work, &["config", "user.email", "src@example.com"]);
	std::fs::write(work.join("hello.txt"), b"world\n").unwrap();
	git(&work, &["add", "hello.txt"]);
	git(&work, &["commit", "-m", "first"]);
	git(&work, &["branch", "feature"]);
	std::fs::write(work.join("second.txt"), b"more\n").unwrap();
	git(&work, &["add", "second.txt"]);
	git(&work, &["commit", "-m", "second"]);
	// Canonicalise so macOS `/tmp` → `/private/tmp` matches the path git-upload-pack resolves.
	std::fs::canonicalize(&work).unwrap()
}

/// The environment that makes a clone hermetic (no ambient global/system git config) and routes ssh
/// through the fake, so the test never touches the developer's real config or a network.
fn clone_env(fake_ssh: &str) -> Vec<(&str, &str)> {
	vec![
		("GIT_SSH_COMMAND", fake_ssh),
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
	]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_over_ssh_url_checks_out_the_repo() {
	let source = make_source("sha1");
	let scripts = unique_tmp("ssh-scripts-url");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();

	let dst = unique_tmp("ssh-dst-url");
	let target = dst.join("clone");
	let url = format!("ssh://git@localhost{}", source.display());
	let out = gta_env(
		&["clone", &url, target.to_str().unwrap()],
		&clone_env(fake_ssh),
	)
	.await;
	gta_ok(&out, "clone over ssh");

	// The working tree and refs match the stock-git source.
	assert_eq!(std::fs::read(target.join("hello.txt")).unwrap(), b"world\n");
	assert_eq!(std::fs::read(target.join("second.txt")).unwrap(), b"more\n");
	assert_eq!(
		git(&target, &["rev-parse", "HEAD"]),
		git(&source, &["rev-parse", "main"]),
	);
	assert_eq!(
		git(&target, &["rev-parse", "refs/heads/feature"]),
		git(&source, &["rev-parse", "feature"]),
	);
	// The SSH URL is persisted verbatim as the origin.
	assert_eq!(git(&target, &["config", "remote.origin.url"]), url);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_over_scp_alias_checks_out_the_repo() {
	let source = make_source("sha1");
	let scripts = unique_tmp("ssh-scripts-scp");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();

	let dst = unique_tmp("ssh-dst-scp");
	let target = dst.join("clone");
	// scp-like alias: `[user@]host:path`. The path after the colon is sent verbatim (absolute here so
	// the fake ssh resolves it), matching git's scp handling.
	let url = format!("git@localhost:{}", source.display());
	let out = gta_env(
		&["clone", &url, target.to_str().unwrap()],
		&clone_env(fake_ssh),
	)
	.await;
	gta_ok(&out, "clone over scp alias");

	assert_eq!(std::fs::read(target.join("hello.txt")).unwrap(), b"world\n");
	assert_eq!(
		git(&target, &["rev-parse", "HEAD"]),
		git(&source, &["rev-parse", "main"]),
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_over_ssh_handles_an_empty_repository() {
	// An empty remote advertises no refs: gitana must still finalise the session (send the terminating
	// flush and await ssh) so upload-pack exits cleanly instead of logging "the remote end hung up",
	// matching git's clean empty clone.
	let source = unique_tmp("ssh-src-empty");
	assert!(
		std::process::Command::new("git")
			.args(["init", "--bare", "-b", "main", source.to_str().unwrap()])
			.output()
			.expect("git init --bare")
			.status
			.success()
	);
	let source = std::fs::canonicalize(&source).unwrap();
	let scripts = unique_tmp("ssh-scripts-empty");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();

	let dst = unique_tmp("ssh-dst-empty");
	let target = dst.join("clone");
	let url = format!("ssh://git@localhost{}", source.display());
	let out = gta_env(
		&["clone", &url, target.to_str().unwrap()],
		&clone_env(fake_ssh),
	)
	.await;
	gta_ok(&out, "clone empty over ssh");
	// A clean empty clone: HEAD points at the default branch, no refs yet.
	assert_eq!(
		std::fs::read_to_string(target.join(".git/HEAD"))
			.unwrap()
			.trim(),
		"ref: refs/heads/main",
	);
	// The server did not hang up unexpectedly (the terminating flush was sent).
	assert!(
		!String::from_utf8_lossy(&out.stderr).contains("hung up"),
		"empty clone left the ssh session unfinished: {}",
		String::from_utf8_lossy(&out.stderr),
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_refuses_option_injection_hostname() {
	// A URL whose host begins with `-` must be refused (git's CVE-2017-1000117 guard) rather than
	// passed to `ssh` as an option. The clone fails and the target is never created.
	let scripts = unique_tmp("ssh-scripts-inject");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();

	let dst = unique_tmp("ssh-dst-inject");
	let target = dst.join("clone");
	let out = gta_env(
		&[
			"clone",
			"ssh://-oProxyCommand=payload/repo.git",
			target.to_str().unwrap(),
		],
		&clone_env(fake_ssh),
	)
	.await;
	assert!(!out.status.success(), "malicious host must be refused");
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("strange hostname"),
		"expected a 'strange hostname' rejection, got: {}",
		String::from_utf8_lossy(&out.stderr),
	);
	assert!(
		!target.exists(),
		"no checkout should be created for a refused clone"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_over_ssh_negotiates_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: stock git lacks SHA-256 repository support");
		return;
	}
	let source = make_source("sha256");
	let scripts = unique_tmp("ssh-scripts-256");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();

	let dst = unique_tmp("ssh-dst-256");
	let target = dst.join("clone");
	let url = format!("ssh://git@localhost{}", source.display());
	let out = gta_env(
		&["clone", &url, target.to_str().unwrap()],
		&clone_env(fake_ssh),
	)
	.await;
	gta_ok(&out, "clone over ssh (sha256)");

	assert_eq!(std::fs::read(target.join("hello.txt")).unwrap(), b"world\n");
	// The clone adopted the remote's object format.
	assert_eq!(
		git(&target, &["rev-parse", "--show-object-format"]),
		"sha256",
	);
	assert_eq!(
		git(&target, &["rev-parse", "HEAD"]),
		git(&source, &["rev-parse", "main"]),
	);
}
