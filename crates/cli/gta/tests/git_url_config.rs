//! `url.*.insteadOf`/`pushInsteadOf` rewriting and `http.extraHeader` for the `gta` client, end to end.
//!
//! A loopback axum server ([`support::serve_gitana`]) serves gitana's own Smart-HTTP handlers. The real
//! `gta` binary (subprocess) is pointed at a *fictional* URL that `url.*.insteadOf` rewrites to the real
//! server, and — separately — at a server that `400`s unless the request carries the exact
//! `http.extraHeader` the client is configured to send. Every case neutralises the ambient
//! global/system gitconfig so a developer's real config never leaks in.

mod support;

use std::path::Path;

use gitana_object::Sha256;
use gitana_repository::{FileMode, TreeBuildEntry};
use support::{ServerHash, gta_env, open, serve_gitana, serve_gitana_require_header, unique_tmp};

/// A fixed identity for the server-side seed commit (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

/// Record a commit of `file` = `content` on the server repo's `HEAD` branch.
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

/// Seed a one-commit server repo at `git_dir`.
async fn seed(git_dir: &Path) {
	std::fs::create_dir_all(git_dir).unwrap();
	open::<Sha256>(git_dir).init().await.unwrap();
	commit_file(git_dir, "hello.txt", b"hello\n").await;
}

/// `gta` with the ambient global/system gitconfig neutralised, plus extra `env` (applied last, so a
/// test may re-point `GIT_CONFIG_GLOBAL` at its own config file).
async fn gta_iso(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
	let mut full = vec![
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
	];
	full.extend_from_slice(env);
	gta_env(args, &full).await
}

/// A `url.<real>.insteadOf = <fake>` rewrite in the ambient config lets a clone of the *fake* URL reach
/// the real server. git rewrites the transport URL before use; so does gta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_rewrites_url_with_insteadof() {
	let work = unique_tmp("url-insteadof");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let real = serve_gitana(git_dir, ServerHash::Sha256).await;
	let fake = "https://fake.invalid";

	// A global config that rewrites the fake URL to the real server (which serves at its root).
	let global = work.join("global.gitconfig");
	std::fs::write(&global, format!("[url \"{real}\"]\n\tinsteadOf = {fake}\n")).unwrap();

	let checkout = work.join("c");
	let out = gta_iso(
		&["clone", fake, checkout.to_str().unwrap()],
		&[("GIT_CONFIG_GLOBAL", global.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"insteadOf clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(
		checkout.join("hello.txt").exists(),
		"checkout missing file — rewrite did not reach the real server"
	);

	// git persists the ORIGINAL clone URL, not the rewritten transport URL, so a later change to the
	// rewrite rules still applies.
	let config = std::fs::read_to_string(checkout.join(".git/config")).unwrap();
	assert!(
		config.contains(&format!("url = {fake}")),
		"clone should persist the original URL, got:\n{config}"
	);
	assert!(
		!config.contains(&real),
		"clone must not persist the rewritten transport URL:\n{config}"
	);
}

/// A `fetch` rewrites `remote.origin.url` through `url.*.insteadOf` from the merged config, so a remote
/// saved as a fictional URL still reaches the real server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_rewrites_remote_url_with_insteadof() {
	let work = unique_tmp("url-insteadof-fetch");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let real = serve_gitana(git_dir, ServerHash::Sha256).await;

	// A client whose origin is the fictional URL, with a local insteadOf rewrite to the real server.
	let client = work.join("client");
	assert!(
		gta_iso(&["init", client.to_str().unwrap()], &[])
			.await
			.status
			.success()
	);
	let config = client.join(".git/config");
	let existing = std::fs::read_to_string(&config).unwrap_or_default();
	std::fs::write(
		&config,
		format!(
			"{existing}\
			 [remote \"origin\"]\n\turl = https://fake.invalid\n\
			 \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
			 [url \"{real}\"]\n\tinsteadOf = https://fake.invalid\n"
		),
	)
	.unwrap();

	let out = gta_iso(&["-C", client.to_str().unwrap(), "fetch"], &[]).await;
	assert!(
		out.status.success(),
		"insteadOf fetch failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(
		client.join(".git/refs/remotes/origin/main").exists(),
		"tracking ref missing — rewrite did not reach the real server"
	);
}

/// A configured `http.extraHeader` is sent on every request: the server `400`s without it, so a
/// successful clone proves the client attached it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_sends_configured_extra_header() {
	let work = unique_tmp("extra-header");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_require_header(git_dir, ServerHash::Sha256, "X-Extra", "hello").await;

	let global = work.join("global.gitconfig");
	std::fs::write(&global, "[http]\n\textraHeader = X-Extra: hello\n").unwrap();

	let checkout = work.join("c");
	let out = gta_iso(
		&["clone", &url, checkout.to_str().unwrap()],
		&[("GIT_CONFIG_GLOBAL", global.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"clone with extraHeader failed (header not sent?): {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");
}

/// Without the configured header, the same server rejects the clone — proving the gate is real (and
/// that the previous test's success was due to the header, not an ungated server).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_without_required_header_is_rejected() {
	let work = unique_tmp("extra-header-missing");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_require_header(git_dir, ServerHash::Sha256, "X-Extra", "hello").await;

	let checkout = work.join("c");
	let out = gta_iso(&["clone", &url, checkout.to_str().unwrap()], &[]).await;
	assert!(
		!out.status.success(),
		"clone should fail when the required header is not configured"
	);
}
