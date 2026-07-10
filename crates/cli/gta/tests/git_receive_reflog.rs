//! Reflog parity for server-side receive-pack (push): when a push lands, gitana's receive-pack writes
//! the `push` reflog entries git does — `logs/HEAD` (mirrored when HEAD points at the pushed branch),
//! `logs/refs/heads/*`, credited to the *server's* committer identity — byte-for-byte with stock git.
//!
//! Both servers are bare git repos with `core.logAllRefUpdates=true` (git's receive-pack writes no
//! reflog for a bare repo under the default config; the setting turns it on). The gitana server is
//! served by gitana's own Smart-HTTP handlers ([`support::serve_gitana_with_reflog`]) crediting a
//! fixed server identity, and a `gta` client pushes to it; the git server receives the same pushes
//! over the file transport from a `git` client whose `GIT_COMMITTER_*` env is that same identity — so
//! the server credits an identical committer line. After each scenario the two servers' `logs/` trees
//! are compared verbatim.
//!
//! git hardcodes the receive-pack reflog message to `push` (it does not honour `GIT_REFLOG_ACTION`
//! server-side), so there is no action env to juggle: old/new ids, committer line, and message are all
//! compared as-is.
//!
//! SHA-1 only — git's default object format, and the reflog logic under test (the `update_ref` gating
//! and split-HEAD cascade, and `delete_ref`'s reflog removal) is hash-agnostic and already covered for
//! both formats by the local reflog oracle (`git_reflog.rs`).
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use support::{ServerHash, gta, gta_ok, serve_gitana_with_reflog};

const NAME: &str = "A U Thor";
const EMAIL: &str = "a@example.com";
const DATE: &str = "1700000000 +0000";

/// The fixed identity and date. The client commits under it (so `gta` and `git` push identical
/// object ids), and — pushed to the git server over the file transport — it is also the server's
/// committer identity, the one git credits receive-pack reflogs to.
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

/// The git reflog committer line the server credits: `Name <email> secs ±hhmm`.
fn committer_line() -> String {
	format!("{NAME} <{EMAIL}> {DATE}")
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

/// Run `git` on the runtime-blocking pool (so a server task keeps serving) — used for a client push
/// to the git server, which forks a `git receive-pack` that inherits the committer identity.
async fn git_off_runtime(dir: PathBuf, args: Vec<String>) -> Output {
	tokio::task::spawn_blocking(move || {
		let refs: Vec<&str> = args.iter().map(String::as_str).collect();
		git_out(&dir, &refs)
	})
	.await
	.unwrap()
}

/// Initialise a bare git server repo at `path` with `main` as its initial branch and
/// `core.logAllRefUpdates=true`, so receive-pack writes reflogs (a bare repo's default is off).
fn init_bare_server(path: &Path, log_all: bool) {
	git(
		path.parent().unwrap(),
		&["init", "-q", "--bare", "-b", "main", path.to_str().unwrap()],
	);
	if log_all {
		git(path, &["config", "core.logAllRefUpdates", "true"]);
	}
}

/// Every file under `<repo>/logs` as `relative path → bytes`, so two servers' reflog trees compare
/// verbatim. Missing `logs/` yields an empty map (nothing was logged).
fn logs_tree(repo: &Path) -> BTreeMap<String, Vec<u8>> {
	let root = repo.join("logs");
	let mut out = BTreeMap::new();
	let mut stack = vec![root.clone()];
	while let Some(dir) = stack.pop() {
		let Ok(entries) = std::fs::read_dir(&dir) else {
			continue;
		};
		for entry in entries {
			let path = entry.unwrap().path();
			if path.is_dir() {
				stack.push(path);
			} else {
				let rel = path
					.strip_prefix(&root)
					.unwrap()
					.to_string_lossy()
					.into_owned();
				out.insert(rel, std::fs::read(&path).unwrap());
			}
		}
	}
	out
}

/// Assert the two servers' `logs/` trees are byte-identical, rendering any difference readably.
fn assert_logs_match(git_srv: &Path, gitana_srv: &Path) {
	let want = logs_tree(git_srv);
	let got = logs_tree(gitana_srv);
	let render = |tree: &BTreeMap<String, Vec<u8>>| {
		tree
			.iter()
			.map(|(k, v)| format!("[{k}]\n{}", String::from_utf8_lossy(v)))
			.collect::<Vec<_>>()
			.join("\n")
	};
	assert_eq!(
		render(&want),
		render(&got),
		"receive-pack reflog trees diverge (git=left, gitana=right)"
	);
}

/// Build a client repo at `<root>/client` with `main` (two commits) and `feature` (branched at the
/// first commit), and configure `origin` to `url` for `gta push`. Returns the client dir.
fn build_client(root: &Path, url: &str) -> PathBuf {
	let client = root.join("client");
	std::fs::create_dir_all(&client).unwrap();
	git(&client, &["init", "-q", "-b", "main", "."]);
	std::fs::write(client.join("a.txt"), b"one\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "one"]);
	git(&client, &["branch", "feature"]);
	std::fs::write(client.join("a.txt"), b"two\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "two"]);
	git(&client, &["remote", "add", "origin", url]);
	client
}

/// Push `refspecs` to both servers: `git` (over the file transport, forking a server-side
/// receive-pack under the committer identity) and `gta` (over HTTP to the gitana server). `flags` are
/// extra client push flags (e.g. `-f`) passed to both.
async fn push_both(client: &Path, git_srv: &Path, flags: &[&str], refspecs: &[&str]) {
	let mut git_args = vec!["push".to_owned()];
	git_args.extend(flags.iter().map(|s| (*s).to_owned()));
	git_args.push(git_srv.to_str().unwrap().to_owned());
	git_args.extend(refspecs.iter().map(|s| (*s).to_owned()));
	let out = git_off_runtime(client.to_path_buf(), git_args).await;
	assert!(
		out.status.success(),
		"git push failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	let mut gta_args = vec!["-C", client.to_str().unwrap(), "push"];
	gta_args.extend_from_slice(flags);
	gta_args.push("origin");
	gta_args.extend_from_slice(refspecs);
	gta_ok(&gta(&gta_args).await, "push");
}

/// Create-push of `main` (cascading into `HEAD`) + `feature`, a fast-forward of `main`, and a forced
/// non-fast-forward rewind of `feature` all record `push` reflogs matching git.
#[tokio::test]
async fn push_reflogs_match_git() {
	let root = tmp("recv-reflog");
	let git_srv = root.join("git.git");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&git_srv, true);
	init_bare_server(&gitana_srv, true);
	let url = serve_gitana_with_reflog(gitana_srv.clone(), ServerHash::Sha1, committer_line()).await;
	let client = build_client(&root, &url);

	// Create main (→ logs/HEAD cascade) and feature.
	push_both(&client, &git_srv, &[], &["main", "feature"]).await;
	assert_logs_match(&git_srv, &gitana_srv);

	// Fast-forward main.
	std::fs::write(client.join("a.txt"), b"three\n").unwrap();
	git(&client, &["add", "."]);
	git(&client, &["commit", "-qm", "three"]);
	push_both(&client, &git_srv, &[], &["main"]).await;
	assert_logs_match(&git_srv, &gitana_srv);

	// Force a non-fast-forward rewind of feature (to main's first commit's tree — a divergent tip).
	let first = git(&client, &["rev-parse", "main~2"]);
	git(&client, &["branch", "-f", "feature", &first]);
	push_both(&client, &git_srv, &["-f"], &["feature"]).await;
	assert_logs_match(&git_srv, &gitana_srv);
}

/// Deleting a pushed ref removes its reflog on the server, exactly as git does — no stale `logs/`
/// entry survives.
#[tokio::test]
async fn push_delete_removes_reflog_like_git() {
	let root = tmp("recv-reflog-del");
	let git_srv = root.join("git.git");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&git_srv, true);
	init_bare_server(&gitana_srv, true);
	let url = serve_gitana_with_reflog(gitana_srv.clone(), ServerHash::Sha1, committer_line()).await;
	let client = build_client(&root, &url);

	push_both(&client, &git_srv, &[], &["main", "feature"]).await;
	// Non-vacuous: the create push wrote feature's reflog on both servers.
	assert!(git_srv.join("logs/refs/heads/feature").exists());
	assert!(gitana_srv.join("logs/refs/heads/feature").exists());
	assert_logs_match(&git_srv, &gitana_srv);

	// Delete feature; git removes its reflog with the ref, and so must gitana.
	push_both(&client, &git_srv, &[], &[":feature"]).await;
	assert!(!git_srv.join("logs/refs/heads/feature").exists());
	assert!(!gitana_srv.join("logs/refs/heads/feature").exists());
	assert_logs_match(&git_srv, &gitana_srv);
}

/// Deleting the branch `HEAD` points at cascades a `<old> <zero>` deletion entry into `logs/HEAD`
/// (git's split-HEAD update), on top of removing the branch's own reflog. gitana requires only the
/// `force` (admin) grant to delete a current branch, where stock git also needs
/// `receive.denyDeleteCurrent=ignore` — so that is set on the git oracle.
#[tokio::test]
async fn push_delete_current_branch_cascades_head_reflog() {
	let root = tmp("recv-reflog-delcur");
	let git_srv = root.join("git.git");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&git_srv, true);
	init_bare_server(&gitana_srv, true);
	// git refuses to delete HEAD's branch unless told otherwise; gitana has no such guard.
	git(&git_srv, &["config", "receive.denyDeleteCurrent", "ignore"]);
	let url = serve_gitana_with_reflog(gitana_srv.clone(), ServerHash::Sha1, committer_line()).await;
	let client = build_client(&root, &url);

	// main is HEAD's branch on both bare servers (`git init --bare -b main`).
	push_both(&client, &git_srv, &[], &["main"]).await;
	assert_logs_match(&git_srv, &gitana_srv);

	// Delete main: its own reflog goes, and logs/HEAD gains a `<old> <zero> … push` deletion line.
	push_both(&client, &git_srv, &[], &[":main"]).await;
	assert!(!git_srv.join("logs/refs/heads/main").exists());
	assert!(!gitana_srv.join("logs/refs/heads/main").exists());
	// Non-vacuous: git recorded the HEAD deletion entry (a second logs/HEAD line).
	let head_log = std::fs::read(git_srv.join("logs/HEAD")).unwrap();
	assert_eq!(
		String::from_utf8_lossy(&head_log).lines().count(),
		2,
		"git should have a create + a delete entry in logs/HEAD"
	);
	assert_logs_match(&git_srv, &gitana_srv);
}

/// When the server can move a ref but not write its reflog (a directory/file conflict in `logs/`),
/// receive-pack rolls the ref back and reports the rejection — git rejects such an update without
/// moving the ref, rather than leaving the branch advanced while the client is told it failed.
#[tokio::test]
async fn push_reflog_write_failure_rolls_back_the_ref() {
	let root = tmp("recv-reflog-rollback");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&gitana_srv, true);
	let url = serve_gitana_with_reflog(gitana_srv.clone(), ServerHash::Sha1, committer_line()).await;
	let client = build_client(&root, &url);

	// Establish `logs/refs/heads/` on the server (push main).
	let c = client.to_str().unwrap();
	gta_ok(
		&gta(&["-C", c, "push", "origin", "main"]).await,
		"push main",
	);

	// Plant a stray reflog *file* where `refs/heads/foo/bar`'s reflog directory must go, so the
	// server's post-move reflog append hits a directory/file conflict.
	std::fs::write(gitana_srv.join("logs/refs/heads/foo"), b"stray\n").unwrap();

	// Create refs/heads/foo/bar: the ref write succeeds, the reflog append fails. The server must undo
	// the move and reject, not report a false success with the branch left in place.
	let out = gta(&["-C", c, "push", "origin", "main:refs/heads/foo/bar"]).await;
	assert!(
		!out.status.success(),
		"a reflog-write failure must be reported as a rejection: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(
		!gitana_srv.join("refs/heads/foo/bar").exists(),
		"the ref must be rolled back when its reflog cannot be written"
	);
}

/// Under a bare server's default config (`core.logAllRefUpdates` unset), receive-pack writes no
/// reflog — gitana honours the same gating even with a server identity configured.
#[tokio::test]
async fn push_without_logallrefupdates_writes_no_reflog() {
	let root = tmp("recv-reflog-off");
	let git_srv = root.join("git.git");
	let gitana_srv = root.join("gitana.git");
	init_bare_server(&git_srv, false);
	init_bare_server(&gitana_srv, false);
	let url = serve_gitana_with_reflog(gitana_srv.clone(), ServerHash::Sha1, committer_line()).await;
	let client = build_client(&root, &url);

	push_both(&client, &git_srv, &[], &["main", "feature"]).await;
	assert!(
		logs_tree(&git_srv).is_empty(),
		"git wrote a reflog under the default config"
	);
	assert_logs_match(&git_srv, &gitana_srv);
}
