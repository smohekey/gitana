//! End-to-end `gta fetch` / `gta push` over the SSH transport, against stock `git`.
//!
//! A fake `ssh` (via `GIT_SSH_COMMAND`) runs the remote `git-upload-pack` / `git-receive-pack` locally
//! against a stock-`git` bare repository, so these exercise gitana's SSH client — the stateful
//! `multi_ack_detailed` fetch negotiation and the receive-pack push — against a real git server.

mod support;

use std::path::{Path, PathBuf};

use support::{git, git_try, gta_env, gta_ok, unique_tmp};

/// A fake `ssh` that runs the remote git command (its last argument) locally, ignoring host/options.
fn write_fake_ssh(dir: &Path) -> PathBuf {
	let script = dir.join("fake-ssh.sh");
	std::fs::write(
		&script,
		"#!/bin/sh\nfor a in \"$@\"; do cmd=\"$a\"; done\neval \"$cmd\"\n",
	)
	.unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
	}
	script
}

/// The hermetic env for a gta remote op over the fake ssh (no ambient global/system git config).
fn ssh_env(fake_ssh: &str) -> Vec<(&str, &str)> {
	vec![
		("GIT_SSH_COMMAND", fake_ssh),
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
		("GIT_AUTHOR_NAME", "Dev"),
		("GIT_AUTHOR_EMAIL", "dev@example.com"),
		("GIT_COMMITTER_NAME", "Dev"),
		("GIT_COMMITTER_EMAIL", "dev@example.com"),
	]
}

/// Configure the identity of a stock-git repo and stamp a commit adding `file`=`content`.
fn git_commit(dir: &Path, file: &str, content: &str, message: &str) {
	git(dir, &["config", "user.name", "Src"]);
	git(dir, &["config", "user.email", "src@example.com"]);
	std::fs::write(dir.join(file), content).unwrap();
	git(dir, &["add", file]);
	git(dir, &["commit", "-m", message]);
}

/// Create a bare stock-git source with one commit on `main`, plus a work clone to advance it.
/// Returns `(bare source absolute path, work checkout)`.
fn seed(tag: &str) -> (PathBuf, PathBuf) {
	let base = unique_tmp(tag);
	let source = base.join("source.git");
	let work = base.join("work");
	assert!(
		std::process::Command::new("git")
			.args(["init", "--bare", "-b", "main", source.to_str().unwrap()])
			.output()
			.expect("git init --bare")
			.status
			.success()
	);
	let source = std::fs::canonicalize(&source).unwrap();
	assert!(
		std::process::Command::new("git")
			.args(["clone", source.to_str().unwrap(), work.to_str().unwrap()])
			.output()
			.expect("git clone")
			.status
			.success()
	);
	git_commit(&work, "f.txt", "one\n", "one");
	git(&work, &["push", "origin", "main"]);
	(source, work)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_over_ssh_downloads_new_commits() {
	let (source, work) = seed("ssh-fp-fetch");
	let scripts = unique_tmp("ssh-fp-fetch-scripts");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();
	let env = ssh_env(fake_ssh);

	// Clone the source over SSH: the client now has `main` at the first commit.
	let dst = unique_tmp("ssh-fp-fetch-dst");
	let client = dst.join("client");
	let url = format!("ssh://git@localhost{}", source.display());
	gta_ok(
		&gta_env(&["clone", &url, client.to_str().unwrap()], &env).await,
		"clone over ssh",
	);

	// Advance the source with a second commit.
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);
	let want = git(&source, &["rev-parse", "main"]);

	// Fetch over SSH: the stateful negotiation offers the client's tip as a `have`, and the server sends
	// only the new commit's objects.
	let out = gta_env(&["-C", client.to_str().unwrap(), "fetch"], &env).await;
	gta_ok(&out, "fetch over ssh");

	// The tracking ref advanced to the source's new tip.
	let tracking = git(&client, &["rev-parse", "refs/remotes/origin/main"]);
	assert_eq!(tracking, want, "fetch did not advance the tracking ref");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_over_ssh_negotiates_across_have_groups() {
	// Force git's stateful negotiation past a single have-group: the client commits 17 local commits (>
	// the 16-have batch) with no tracking ref for the common base, so its `have`s span two rounds — the
	// first group (all client-only) is NAK'd, the second reaches the common ancestor and the server is
	// `ready`. This exercises the multi-round loop, not just the single-round case.
	let (source, work) = seed("ssh-fp-multiround");
	let scripts = unique_tmp("ssh-fp-multiround-scripts");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();
	let env = ssh_env(fake_ssh);

	let dst = unique_tmp("ssh-fp-multiround-dst");
	let client = dst.join("client");
	let url = format!("ssh://git@localhost{}", source.display());
	gta_ok(
		&gta_env(&["clone", &url, client.to_str().unwrap()], &env).await,
		"clone over ssh",
	);
	// 17 local commits on `main` (client-only history), then drop the origin tracking ref so the common
	// base is not offered as an early have.
	for i in 0..17 {
		git_commit(
			&client,
			"local.txt",
			&format!("line {i}\n"),
			&format!("c{i}"),
		);
	}
	git(&client, &["update-ref", "-d", "refs/remotes/origin/main"]);

	// Advance the source so there is something to fetch.
	git_commit(&work, "f.txt", "one\ntwo\n", "two");
	git(&work, &["push", "origin", "main"]);
	let want = git(&source, &["rev-parse", "main"]);

	gta_ok(
		&gta_env(&["-C", client.to_str().unwrap(), "fetch"], &env).await,
		"multi-round fetch over ssh",
	);
	assert_eq!(
		git(&client, &["rev-parse", "refs/remotes/origin/main"]),
		want,
		"multi-round fetch did not advance the tracking ref",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_over_ssh_is_up_to_date() {
	// A fetch with nothing new negotiates cleanly (empty wants → finalize) and exits 0.
	let (source, _work) = seed("ssh-fp-uptodate");
	let scripts = unique_tmp("ssh-fp-uptodate-scripts");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();
	let env = ssh_env(fake_ssh);

	let dst = unique_tmp("ssh-fp-uptodate-dst");
	let client = dst.join("client");
	let url = format!("ssh://git@localhost{}", source.display());
	gta_ok(
		&gta_env(&["clone", &url, client.to_str().unwrap()], &env).await,
		"clone over ssh",
	);
	gta_ok(
		&gta_env(&["-C", client.to_str().unwrap(), "fetch"], &env).await,
		"up-to-date fetch over ssh",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_over_ssh_advances_the_remote() {
	let (source, _work) = seed("ssh-fp-push");
	let scripts = unique_tmp("ssh-fp-push-scripts");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();
	let env = ssh_env(fake_ssh);

	// Clone over SSH, make a local commit, push it back over SSH.
	let dst = unique_tmp("ssh-fp-push-dst");
	let client = dst.join("client");
	let url = format!("ssh://git@localhost{}", source.display());
	gta_ok(
		&gta_env(&["clone", &url, client.to_str().unwrap()], &env).await,
		"clone over ssh",
	);
	let c = client.to_str().unwrap();
	std::fs::write(client.join("local.txt"), "local change\n").unwrap();
	gta_ok(&gta_env(&["-C", c, "add", "local.txt"], &env).await, "add");
	gta_ok(
		&gta_env(&["-C", c, "commit", "-m", "local"], &env).await,
		"commit",
	);
	let pushed = git(&client, &["rev-parse", "HEAD"]);

	let out = gta_env(&["-C", c, "push"], &env).await;
	gta_ok(&out, "push over ssh");

	// The bare source's `main` now points at the pushed commit — and git can read the pushed objects.
	assert_eq!(
		git(&source, &["rev-parse", "main"]),
		pushed,
		"push did not advance the remote ref",
	);
	assert!(
		git_try(&source, &["cat-file", "-e", &pushed])
			.status
			.success(),
		"the pushed commit object is missing on the remote",
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_up_to_date_over_ssh_finalizes_cleanly() {
	// A push with nothing to send must still finalize the SSH session (terminating flush + exit status),
	// so git-receive-pack exits cleanly instead of logging "the remote end hung up unexpectedly".
	let (source, _work) = seed("ssh-fp-push-uptodate");
	let scripts = unique_tmp("ssh-fp-push-uptodate-scripts");
	let fake_ssh = write_fake_ssh(&scripts);
	let fake_ssh = fake_ssh.to_str().unwrap();
	let env = ssh_env(fake_ssh);

	let dst = unique_tmp("ssh-fp-push-uptodate-dst");
	let client = dst.join("client");
	let url = format!("ssh://git@localhost{}", source.display());
	gta_ok(
		&gta_env(&["clone", &url, client.to_str().unwrap()], &env).await,
		"clone over ssh",
	);
	// Nothing committed locally — `main` already matches the remote, so the push is up-to-date.
	let out = gta_env(&["-C", client.to_str().unwrap(), "push"], &env).await;
	gta_ok(&out, "up-to-date push over ssh");
	assert!(
		!String::from_utf8_lossy(&out.stderr).contains("hung up"),
		"the up-to-date push left the ssh session unfinished: {}",
		String::from_utf8_lossy(&out.stderr),
	);
}
