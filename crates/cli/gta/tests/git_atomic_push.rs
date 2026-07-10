//! Atomic-push parity with stock git. gitana's receive-pack honours git's `--atomic` capability — a
//! push carrying a ref it must reject applies *nothing* — and gitana's `gta push --atomic` client
//! requests it. Both are proven against stock git in one shot: a `git` client pushes to a bare `git`
//! server (`receive.denyNonFastForwards`, so a non-fast-forward ref is rejected), a `gta` client
//! pushes the same refspecs to a gitana server (served with the destructive-update grant off, the
//! same rejection), and the two servers' ref state is compared after each push.
//!
//! The push mixes a good ref (a fast-forward of `main`) with a bad one (a forced non-fast-forward of
//! `topic`). Under `--atomic` both servers must leave *both* refs untouched; under the default
//! per-ref push both must land `main` and reject `topic`.
//!
//! SHA-1 only — git's default format; the atomic logic under test (`RefStore::transact` and the
//! receive-pack capability plumbing) is hash-agnostic.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use support::{ServerHash, gta, serve_gitana_no_force};

const NAME: &str = "A U Thor";
const EMAIL: &str = "a@example.com";
const DATE: &str = "1700000000 +0000";

/// The fixed identity and date, so `git` and `gta` build identical object ids.
fn identity() -> [(&'static str, &'static str); 6] {
	[
		("GIT_AUTHOR_NAME", NAME),
		("GIT_AUTHOR_EMAIL", EMAIL),
		("GIT_AUTHOR_DATE", DATE),
		("GIT_COMMITTER_NAME", NAME),
		("GIT_COMMITTER_EMAIL", EMAIL),
		("GIT_COMMITTER_DATE", DATE),
	]
}

/// A fresh, unique temp dir named for `tag`.
fn tmp(tag: &str) -> PathBuf {
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Run `git -C dir <args>` under the fixed identity, asserting success.
fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_out(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Run `git -C dir <args>` under the fixed identity, returning the raw output.
fn git_out(dir: &Path, args: &[&str]) -> Output {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.envs(identity())
		.output()
		.expect("run git")
}

/// Run `git` on the runtime-blocking pool (so the gitana server task keeps serving) — used for a
/// client push to the git server, which forks a `git receive-pack`.
async fn git_off_runtime(dir: PathBuf, args: Vec<String>) -> Output {
	tokio::task::spawn_blocking(move || {
		let refs: Vec<&str> = args.iter().map(String::as_str).collect();
		git_out(&dir, &refs)
	})
	.await
	.unwrap()
}

/// Initialise a bare git server at `path` with `main` as its initial branch and
/// `receive.denyNonFastForwards`, so a non-fast-forward ref move is rejected (mirroring the gitana
/// server's destructive-update grant being off).
fn init_bare_server(path: &Path) {
	git(
		path.parent().unwrap(),
		&["init", "-q", "--bare", "-b", "main", path.to_str().unwrap()],
	);
	git(path, &["config", "receive.denyNonFastForwards", "true"]);
}

/// A bare server's tip for `branch`, or `None` when the ref is absent.
fn server_tip(server: &Path, branch: &str) -> Option<String> {
	let out = git_out(server, &["rev-parse", "--verify", "--quiet", branch]);
	out
		.status
		.success()
		.then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

/// Assert the two servers agree on `branch`'s tip (both moved, or both did not).
fn assert_tips_match(git_srv: &Path, gitana_srv: &Path, branch: &str) {
	assert_eq!(
		server_tip(git_srv, branch),
		server_tip(gitana_srv, branch),
		"servers disagree on {branch}"
	);
}

/// Push `refspecs` (with `flags`) to the git server (over the file transport) and, in parallel, the
/// same to the gitana server via `gta push origin` (over HTTP). Returns whether each push succeeded.
async fn push_both(
	client: &Path,
	git_srv: &Path,
	flags: &[&str],
	refspecs: &[&str],
) -> (bool, bool) {
	let mut git_args = vec!["push".to_owned()];
	git_args.extend(flags.iter().map(|s| (*s).to_owned()));
	git_args.push(git_srv.to_str().unwrap().to_owned());
	git_args.extend(refspecs.iter().map(|s| (*s).to_owned()));
	let git_ok = git_off_runtime(client.to_path_buf(), git_args)
		.await
		.status
		.success();

	let mut gta_args = vec!["-C", client.to_str().unwrap(), "push"];
	gta_args.extend_from_slice(flags);
	gta_args.push("origin");
	gta_args.extend_from_slice(refspecs);
	let gta_ok = gta(&gta_args).await.status.success();

	(git_ok, gta_ok)
}

/// Two bare servers (a `git` one, a gitana one — both rejecting non-fast-forwards) plus a client with
/// `main` and `topic` seeded at the initial commit on both, `origin` pointing at gitana. Returns
/// `(git_srv, gitana_srv, client)`.
async fn seeded_pair(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
	let root = tmp(tag);
	let git_srv = root.join("git.git");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&git_srv);
	init_bare_server(&gitana_srv);
	let url = serve_gitana_no_force(gitana_srv.clone(), ServerHash::Sha1).await;

	let client = root.join("client");
	std::fs::create_dir_all(&client).unwrap();
	git(&client, &["init", "-q", "-b", "main", "."]);
	std::fs::write(client.join("a.txt"), b"one\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "one"]);
	git(&client, &["branch", "topic"]);
	git(&client, &["remote", "add", "origin", &url]);

	let (g, t) = push_both(&client, &git_srv, &[], &["main", "topic"]).await;
	assert!(g && t, "seed push should succeed on both servers");
	let base = server_tip(&git_srv, "refs/heads/main");
	assert_eq!(server_tip(&gitana_srv, "refs/heads/main"), base);
	(git_srv, gitana_srv, client)
}

/// An all-good `--atomic` push (a fast-forward of `main` to an existing ref + a create of `feature`)
/// lands *both* refs on both servers — the fast-forward is checked against the objects the same push
/// delivered, not rejected for want of them.
#[tokio::test]
async fn atomic_push_lands_all_good_refs_vs_git() {
	let (git_srv, gitana_srv, client) = seeded_pair("atomic-push-ok").await;

	// Advance main by a fast-forward commit and branch `feature` at the new tip.
	std::fs::write(client.join("a.txt"), b"two\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "two"]);
	git(&client, &["branch", "feature"]);
	let advanced = git(&client, &["rev-parse", "main"]);

	let (git_ok, gta_ok) = push_both(&client, &git_srv, &["--atomic"], &["main", "feature"]).await;
	assert!(git_ok, "git all-good --atomic push must succeed");
	assert!(gta_ok, "gta all-good --atomic push must succeed");
	for branch in ["refs/heads/main", "refs/heads/feature"] {
		assert_eq!(
			server_tip(&gitana_srv, branch),
			Some(advanced.clone()),
			"gitana server should have landed {branch} atomically"
		);
		assert_tips_match(&git_srv, &gitana_srv, branch);
	}
}

/// `--atomic` push with a good and a bad ref: both servers reject the whole batch (nothing moves);
/// the default per-ref push then lands the good ref and rejects the bad one, identically on both.
#[tokio::test]
async fn atomic_push_is_all_or_nothing_vs_git() {
	let (git_srv, gitana_srv, client) = seeded_pair("atomic-push").await;
	let base = server_tip(&git_srv, "refs/heads/main").unwrap();

	// Advance main by a fast-forward commit; rewind topic to a divergent (non-fast-forward) commit by
	// amending the root, so pushing it needs a force and both servers reject it.
	std::fs::write(client.join("a.txt"), b"two\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "two"]);
	let advanced_main = git(&client, &["rev-parse", "main"]);
	git(&client, &["checkout", "-q", "topic"]);
	git(&client, &["commit", "--amend", "-qm", "one (amended)"]);
	git(&client, &["checkout", "-q", "main"]);

	// Atomic push of the good `main` + the bad `+topic`: rejected as a whole on both servers.
	let (git_ok, gta_ok) = push_both(&client, &git_srv, &["--atomic"], &["main", "+topic"]).await;
	assert!(!git_ok, "git --atomic push with a rejected ref must fail");
	assert!(!gta_ok, "gta --atomic push with a rejected ref must fail");
	// Nothing moved: main is still the seed commit on both servers, topic unchanged.
	assert_eq!(
		server_tip(&git_srv, "refs/heads/main"),
		Some(base.clone()),
		"git server moved main despite the atomic rejection"
	);
	assert_eq!(
		server_tip(&gitana_srv, "refs/heads/main"),
		Some(base.clone()),
		"gitana server moved main despite the atomic rejection"
	);
	assert_tips_match(&git_srv, &gitana_srv, "refs/heads/main");
	assert_tips_match(&git_srv, &gitana_srv, "refs/heads/topic");

	// Default (non-atomic) push of the same refspecs: the good `main` lands, `topic` is rejected —
	// per-ref, identically on both servers.
	let (git_ok, gta_ok) = push_both(&client, &git_srv, &[], &["main", "+topic"]).await;
	assert!(
		!git_ok,
		"the per-ref push still reports failure for the rejected topic"
	);
	assert!(
		!gta_ok,
		"the per-ref push still reports failure for the rejected topic"
	);
	assert_eq!(
		server_tip(&git_srv, "refs/heads/main"),
		Some(advanced_main.clone()),
		"git server should have landed the fast-forward of main"
	);
	assert_eq!(
		server_tip(&gitana_srv, "refs/heads/main"),
		Some(advanced_main.clone()),
		"gitana server should have landed the fast-forward of main per-ref"
	);
	// topic was rejected on both, so it still matches the seed.
	assert_tips_match(&git_srv, &gitana_srv, "refs/heads/topic");
	assert_eq!(server_tip(&gitana_srv, "refs/heads/topic"), Some(base));
}
