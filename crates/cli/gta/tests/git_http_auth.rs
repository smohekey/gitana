//! HTTP Basic-auth credential flow for the `gta` client, end to end.
//!
//! A loopback axum server ([`support::serve_gitana_basic_auth`]) serves gitana's own Smart-HTTP
//! handlers behind `401 WWW-Authenticate: Basic`, and the real `gta` binary (subprocess) must acquire
//! and present credentials to get through — from the URL userinfo, from a saved `remote.origin.url`
//! username plus a scripted `GIT_ASKPASS`, and correctly *failing* on a wrong password. This is the
//! oracle for slice 1 of the HTTP-credentials work.

mod support;

use std::path::Path;

use gitana_object::Sha256;
use gitana_repository::{FileMode, TreeBuildEntry};
use support::{ServerHash, gta, gta_env, open, serve_gitana_basic_auth, unique_tmp};

/// A fixed identity for the server-side seed commit (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

/// Initialise a SHA-256 server repo at `git_dir` with a single commit holding `hello.txt`.
async fn seed(git_dir: &Path) {
	std::fs::create_dir_all(git_dir).unwrap();
	let repo = open::<Sha256>(git_dir);
	repo.init().await.unwrap();
	commit_file(git_dir, "hello.txt", b"hello\n").await;
}

/// Add a commit on the server repo's HEAD introducing `file` with `content`.
async fn commit_file(git_dir: &Path, file: &str, content: &[u8]) {
	let repo = open::<Sha256>(git_dir);
	let blob = repo.write_blob(content).await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: file.to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	repo.commit_on_head(tree, WHO, WHO, "srv\n").await.unwrap();
}

/// Insert `userinfo` (`user` or `user:pass`) into an `http://host…` URL.
fn with_userinfo(url: &str, userinfo: &str) -> String {
	url.replacen("http://", &format!("http://{userinfo}@"), 1)
}

/// Write an executable `askpass` script that echoes `answer` (unix only; the test suite runs on
/// unix). git invokes it as `askpass "<prompt>"` and reads the answer from stdout.
#[cfg(unix)]
fn write_askpass(path: &Path, answer: &str) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::write(path, format!("#!/bin/sh\necho '{answer}'\n")).unwrap();
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A credential embedded in the clone URL as `user:pass@` authenticates the clone, and the saved
/// `remote.origin.url` keeps the username but not the password (matching git).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_authenticates_from_url_userinfo() {
	let work = unique_tmp("auth-url");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let checkout = work.join("c");
	let out = gta(&[
		"clone",
		&with_userinfo(&url, "alice:s3cr3t"),
		checkout.to_str().unwrap(),
	])
	.await;
	assert!(
		out.status.success(),
		"authenticated clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");

	// The saved remote keeps the username as a hint but never the password.
	let config = std::fs::read_to_string(checkout.join(".git/config")).unwrap();
	assert!(
		config.contains("url = http://alice@127.0.0.1"),
		"expected a username-only saved url, got: {config}"
	);
	assert!(
		!config.contains("s3cr3t"),
		"the password leaked into config: {config}"
	);
	// The password must not leak into the clone reflog either (git anonymizes the URL there).
	let reflog = std::fs::read_to_string(checkout.join(".git/logs/HEAD")).unwrap();
	assert!(
		!reflog.contains("s3cr3t"),
		"the password leaked into the reflog: {reflog}"
	);
}

/// With only a username in the URL, the password is prompted for — a scripted `GIT_ASKPASS` supplies
/// it, and the clone succeeds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_prompts_password_via_askpass() {
	let work = unique_tmp("auth-askpass");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let askpass = work.join("askpass.sh");
	write_askpass(&askpass, "s3cr3t");

	let checkout = work.join("c");
	let out = gta_env(
		&[
			"clone",
			&with_userinfo(&url, "alice"),
			checkout.to_str().unwrap(),
		],
		&[("GIT_ASKPASS", askpass.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"askpass clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");
}

/// After an authenticated clone (which saves the username), a later `fetch` re-authenticates: the
/// username comes from the saved remote and the password from `GIT_ASKPASS`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_reauthenticates_with_saved_username_and_askpass() {
	let work = unique_tmp("auth-fetch");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir.clone(), ServerHash::Sha256, "alice", "s3cr3t").await;

	// Clone with full userinfo (persists the `alice` username into remote.origin.url).
	let checkout = work.join("c");
	let out = gta(&[
		"clone",
		&with_userinfo(&url, "alice:s3cr3t"),
		checkout.to_str().unwrap(),
	])
	.await;
	assert!(
		out.status.success(),
		"seed clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	// Advance the server, then fetch — the client must authenticate again, username from the saved
	// remote and password from askpass.
	commit_file(&git_dir, "more.txt", b"more\n").await;
	let askpass = work.join("askpass.sh");
	write_askpass(&askpass, "s3cr3t");
	let out = gta_env(
		&["-C", checkout.to_str().unwrap(), "fetch"],
		&[("GIT_ASKPASS", askpass.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"authenticated fetch failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// A wrong password fails the clone: the server rejects it and — with no way to prompt
/// (`GIT_TERMINAL_PROMPT=0`, no askpass) — the credential provider declines, so the 401 stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_with_wrong_password_is_rejected() {
	let work = unique_tmp("auth-wrong");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let checkout = work.join("c");
	let out = gta_env(
		&[
			"clone",
			&with_userinfo(&url, "alice:wrong"),
			checkout.to_str().unwrap(),
		],
		&[("GIT_TERMINAL_PROMPT", "0")],
	)
	.await;
	assert!(
		!out.status.success(),
		"clone with a wrong password unexpectedly succeeded"
	);
	let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
	assert!(
		stderr.contains("401") || stderr.contains("auth"),
		"expected an auth failure, got: {stderr}"
	);
}
