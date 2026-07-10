//! Reflog parity for over-the-wire clone/fetch: `gta clone` and `gta fetch` write `logs/HEAD` and the
//! remote-tracking reflogs byte-for-byte like stock `git`. A `git http-backend` server ([`support`])
//! serves a bare repo; both a `git` client and a `gta` client run against it under a fixed identity and
//! date, and their reflog files are compared.
//!
//! git derives a fetch's reflog action from the invocation: a plain `git fetch` (no remote named)
//! records `fetch`, and `gta fetch` matches it — its default action is `fetch`, and it honours
//! `GIT_REFLOG_ACTION` like git. So both clients run `fetch` with no env juggling and the whole reflog
//! line — old/new ids, committer, and the `<action>: <status>` message — is compared verbatim.
//!
//! One asymmetry is inherent, not a reflog bug: `gta clone` recreates the advertised branches as local
//! `refs/heads/*` (it does not yet populate `refs/remotes/origin/*`), so a gta client only gains its
//! tracking refs on the *first* `fetch`. git gains them (unlogged) at clone. The per-update reflog
//! *content* still matches; where line counts differ, the update under test is compared by its line.
//!
//! SHA-1 only — git's default, and the format `git http-backend` serves. Gated on `http-backend`.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use support::{git_http_backend_available, serve_git_http_backend};

const NAME: &str = "A U Thor";
const EMAIL: &str = "a@example.com";
const DATE: &str = "1700000000 +0000";

/// The author/committer identity and date, fixed so `gta` and `git` produce identical committer lines
/// (and reflog timestamps) — both read these `GIT_*` variables.
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

/// Run a `git` client command over HTTP off the runtime (so the server task keeps serving), under the
/// fixed identity plus `extra` env (e.g. `GIT_REFLOG_ACTION`).
async fn git_client(args: Vec<String>, extra: Vec<(String, String)>) -> Output {
	tokio::task::spawn_blocking(move || {
		let mut cmd = Command::new("git");
		cmd.args(&args).envs(identity());
		for (k, v) in &extra {
			cmd.env(k, v);
		}
		cmd.output().expect("run git")
	})
	.await
	.unwrap()
}

/// Run a `gta` client command over HTTP off the runtime, under the fixed identity plus `extra` env.
async fn gta_client(args: Vec<String>, extra: Vec<(String, String)>) -> Output {
	tokio::task::spawn_blocking(move || {
		let mut cmd = assert_cmd::Command::cargo_bin("gta").unwrap();
		cmd.args(&args).envs(identity());
		for (k, v) in &extra {
			cmd.env(k, v);
		}
		cmd.output().expect("run gta")
	})
	.await
	.unwrap()
}

fn ok(out: &Output, what: &str) {
	assert!(
		out.status.success(),
		"{what} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// Build `<root>/repo.git`: a bare git repo with `main` (one commit) and `dev` (its own commit, a child
/// of `main`, so `dev` can later be rewound to a non-fast-forward tip). The source work tree stays at
/// `<root>/work` so a test can advance the server.
fn build_bare(root: &Path) {
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "first"]);
	git(&work, &["checkout", "-q", "-b", "dev"]);
	std::fs::write(work.join("d.txt"), b"dev\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "dev work"]);
	git(&work, &["checkout", "-q", "main"]);

	let bare = root.join("repo.git");
	git(
		&work,
		&["clone", "--bare", "-q", ".", bare.to_str().unwrap()],
	);
}

/// Build `<root>/repo.git` with a blob-backed lightweight tag `bt` and an annotated tag `at` (of
/// `main`), for exercising a tag-into-`refs/remotes/*` refspec. Returns `(blob1, at)` ids so a test can
/// later move them.
fn build_bare_with_tags(root: &Path) {
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "first"]);
	// A lightweight tag pointing straight at a blob (peels to no commit) and an annotated tag of `main`.
	std::fs::write(work.join("b.txt"), b"blob one\n").unwrap();
	let blob1 = git(&work, &["hash-object", "-w", "b.txt"]);
	git(&work, &["tag", "bt", &blob1]);
	git(&work, &["tag", "-a", "at", "-m", "annotated one"]);

	let bare = root.join("repo.git");
	git(
		&work,
		&["clone", "--bare", "-q", ".", bare.to_str().unwrap()],
	);
}

/// Run local `git -C dir <args>` under the fixed identity, returning trimmed stdout (asserting success).
fn git(dir: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.envs(identity())
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The whole bytes of a reflog file.
fn reflog(git_dir: &Path, name: &str) -> Vec<u8> {
	let path = git_dir.join("logs").join(name);
	std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The last line of a reflog file (the most recent update).
fn last_line(git_dir: &Path, name: &str) -> String {
	let text = String::from_utf8(reflog(git_dir, name)).unwrap();
	text.lines().last().unwrap().to_owned()
}

fn skip() -> bool {
	if !git_http_backend_available() {
		eprintln!("skipping: git http-backend not available");
		return true;
	}
	false
}

/// `gta clone` records `clone: from <url>` on `logs/HEAD` and the checked-out branch — byte-for-byte
/// like git — and leaves the other recreated branches (git's tracking-ref stand-ins) unlogged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("clone-reflog");
	build_bare(&root);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	// `logs/HEAD` and the checked-out branch's reflog are byte-identical: both a creation
	// (`0000… <tip>`) crediting the fixed committer with `clone: from <url>`.
	assert_eq!(
		reflog(&gta_dir, "HEAD"),
		reflog(&git_dir, "HEAD"),
		"logs/HEAD after clone"
	);
	assert_eq!(
		reflog(&gta_dir, "refs/heads/main"),
		reflog(&git_dir, "refs/heads/main"),
		"logs/refs/heads/main after clone"
	);
	// gta recreates `dev` as a local branch (its stand-in for git's `refs/remotes/origin/dev`); like
	// git's tracking refs, it carries no clone reflog.
	assert!(
		!gta_dir.join("logs/refs/heads/dev").exists(),
		"gta must not reflog the non-HEAD branches it recreates on clone"
	);
}

/// `gta fetch` records `fetch: <status>` on each advanced tracking ref — fast-forward, forced
/// (non-fast-forward under the `+` refspec), and a fresh ref (`storing head`) — matching git's reflog
/// content for the same update.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("fetch-reflog");
	build_bare(&root);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	let gta_c = gta_co.to_str().unwrap().to_owned();
	// git clone populated `refs/remotes/origin/*` (unlogged); an initial gta fetch brings gta level so
	// the *update* below is a like-for-like fast-forward / forced-update in both clients.
	ok(
		&gta_client(vec!["-C".into(), gta_c.clone(), "fetch".into()], vec![]).await,
		"gta initial fetch",
	);

	// Advance the server: `main` fast-forwards, `dev` is rewound to a divergent (non-fast-forward) tip,
	// and a brand-new `feature` branch appears.
	let work = root.join("work");
	std::fs::write(work.join("a.txt"), b"hello\nmore\n").unwrap();
	git(&work, &["commit", "-qam", "second"]);
	let main_new = String::from_utf8(
		Command::new("git")
			.arg("-C")
			.arg(&work)
			.args(["rev-parse", "HEAD"])
			.envs(identity())
			.output()
			.unwrap()
			.stdout,
	)
	.unwrap()
	.trim()
	.to_owned();
	git(&work, &["branch", "feature", "main"]);
	// Rewind dev to a sibling of its old tip (a child of the original `main`), so the tracking-ref
	// update is a genuine non-fast-forward.
	git(&work, &["checkout", "-q", "dev"]);
	git(&work, &["reset", "-q", "--hard", "main~1"]);
	std::fs::write(work.join("d.txt"), b"dev rewritten\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "dev rewrite"]);
	git(&work, &["checkout", "-q", "main"]);
	git(
		&work,
		&[
			"push",
			"-q",
			"--force",
			root.join("repo.git").to_str().unwrap(),
			"main",
			"dev",
			"feature",
		],
	);

	// The git client fetches under gta's action wording; the gta client fetches normally.
	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"git fetch",
	);
	ok(
		&gta_client(vec!["-C".into(), gta_c.clone(), "fetch".into()], vec![]).await,
		"gta fetch",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	// Fast-forward: `main` advanced along its own history.
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/main"),
		last_line(&git_dir, "refs/remotes/origin/main"),
		"origin/main fetch reflog (fast-forward)"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/main").ends_with("fetch: fast-forward"),
		"expected fast-forward status, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/main")
	);
	// Forced update: `dev` rewound to a non-fast-forward tip (accepted under the `+` refspec).
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/dev"),
		last_line(&git_dir, "refs/remotes/origin/dev"),
		"origin/dev fetch reflog (forced-update)"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/dev").ends_with("fetch: forced-update"),
		"expected forced-update status, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/dev")
	);
	// Fresh ref: `feature` is new in both clients, so the whole reflog is a single `storing head` line.
	assert_eq!(
		reflog(&gta_dir, "refs/remotes/origin/feature"),
		reflog(&git_dir, "refs/remotes/origin/feature"),
		"origin/feature fetch reflog (storing head)"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/feature").ends_with("fetch: storing head"),
		"expected storing head status, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/feature")
	);
	// Sanity: the fast-forward landed the advertised new tip.
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/main").contains(&main_new),
		"origin/main reflog should record the advanced tip"
	);
}

/// A refspec that maps tags into a *logged*, non-`refs/tags/*` namespace
/// (`refs/tags/*:refs/remotes/origin/tags/*`, deliberately *unforced*) reflogs each update, and the
/// status word is git's, classified from the object: a blob-backed tag (no commit history) is
/// `storing tag`, an annotated tag advancing along commit history is `fast-forward`. It also proves a
/// moved non-commit tag is *stored* even without `+` (git treats only `refs/tags/*` destinations as
/// immutable) rather than rejected or aborting the fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_refspec_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("tag-refspec-reflog");
	build_bare_with_tags(&root);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	// Both clients fetch tags into a logged, non-`refs/tags/*` namespace, unforced (no `+`).
	for co in [&git_co, &gta_co] {
		git(
			co,
			&[
				"config",
				"--replace-all",
				"remote.origin.fetch",
				"refs/tags/*:refs/remotes/origin/tags/*",
			],
		);
	}
	let git_c = git_co.to_str().unwrap().to_owned();
	let gta_c = gta_co.to_str().unwrap().to_owned();

	// First fetch: both tags are new tracking refs — git words a create as `storing tag` regardless of
	// the object kind (blob or annotated).
	ok(
		&git_client(vec!["-C".into(), git_c.clone(), "fetch".into()], vec![]).await,
		"git tag fetch (create)",
	);
	ok(
		&gta_client(vec!["-C".into(), gta_c.clone(), "fetch".into()], vec![]).await,
		"gta tag fetch (create)",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	for tag in ["bt", "at"] {
		let name = format!("refs/remotes/origin/tags/{tag}");
		assert_eq!(
			reflog(&gta_dir, &name),
			reflog(&git_dir, &name),
			"origin/tags/{tag} reflog after create"
		);
		assert!(
			last_line(&gta_dir, &name).ends_with("fetch: storing tag"),
			"expected storing tag, got: {}",
			last_line(&gta_dir, &name)
		);
	}

	// Move both tags on the server: the blob tag to a *different* blob (still no commit history), the
	// annotated tag forward along `main`'s history (its peeled commit advances — a fast-forward).
	let work = root.join("work");
	std::fs::write(work.join("b2.txt"), b"blob two\n").unwrap();
	let blob2 = git(&work, &["hash-object", "-w", "b2.txt"]);
	git(&work, &["tag", "-f", "bt", &blob2]);
	std::fs::write(work.join("a.txt"), b"hello\nmore\n").unwrap();
	git(&work, &["commit", "-qam", "second"]);
	git(&work, &["tag", "-f", "-a", "at", "-m", "annotated two"]);
	git(
		&work,
		&[
			"push",
			"-q",
			"--force",
			root.join("repo.git").to_str().unwrap(),
			"refs/tags/*:refs/tags/*",
		],
	);

	ok(
		&git_client(vec!["-C".into(), git_c.clone(), "fetch".into()], vec![]).await,
		"git tag fetch (move)",
	);
	ok(
		&gta_client(vec!["-C".into(), gta_c.clone(), "fetch".into()], vec![]).await,
		"gta tag fetch (move)",
	);

	// The blob tag re-store is still `storing tag`; the annotated tag advancing along history is
	// `fast-forward`. Both match git byte-for-byte on the update line.
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/tags/bt"),
		last_line(&git_dir, "refs/remotes/origin/tags/bt"),
		"origin/tags/bt reflog after moving a blob tag"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/tags/bt").ends_with("fetch: storing tag"),
		"moved blob tag should be storing tag, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/tags/bt")
	);
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/tags/at"),
		last_line(&git_dir, "refs/remotes/origin/tags/at"),
		"origin/tags/at reflog after advancing an annotated tag"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/tags/at").ends_with("fetch: fast-forward"),
		"advanced annotated tag should be fast-forward, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/tags/at")
	);
}

/// A fetch refspec whose *source* is neither `refs/heads/*` nor `refs/tags/*` (a Gerrit-style
/// `refs/custom/*`) words a newly stored tracking ref as git's `storing ref` — not `storing head`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_source_ref_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("custom-ref-reflog");
	build_bare(&root);
	// A ref outside `refs/heads` and `refs/tags` on the server, pointing at `main`'s commit.
	let bare = root.join("repo.git");
	git(
		&bare,
		&["update-ref", "refs/custom/thing", "refs/heads/main"],
	);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	for co in [&git_co, &gta_co] {
		git(
			co,
			&[
				"config",
				"--replace-all",
				"remote.origin.fetch",
				"+refs/custom/*:refs/remotes/origin/custom/*",
			],
		);
	}
	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"git custom fetch",
	);
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"gta custom fetch",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	assert_eq!(
		reflog(&gta_dir, "refs/remotes/origin/custom/thing"),
		reflog(&git_dir, "refs/remotes/origin/custom/thing"),
		"origin/custom/thing reflog"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/custom/thing").ends_with("fetch: storing ref"),
		"expected storing ref, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/custom/thing")
	);
}

/// A forced update to an existing `refs/tags/*` *destination* (`+refs/tags/*:refs/tags/*`) with tag
/// reflogs on (`core.logAllRefUpdates=always`) is git's `updating tag` — whatever the object kind or
/// ancestry — distinct from the source-namespace `storing …` wording used elsewhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updating_tag_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("updating-tag-reflog");
	build_bare(&root);
	let work = root.join("work");
	let bare = root.join("repo.git");
	// A lightweight tag on `main`, published to the server.
	git(&work, &["tag", "v1", "main"]);
	git(&work, &["push", "-q", bare.to_str().unwrap(), "v1"]);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	// Enable tag reflogs (tags are not logged by default) and a forced tag refspec, so the tag update is
	// both allowed and recorded.
	for co in [&git_co, &gta_co] {
		git(co, &["config", "core.logAllRefUpdates", "always"]);
		git(
			co,
			&[
				"config",
				"--replace-all",
				"remote.origin.fetch",
				"+refs/tags/*:refs/tags/*",
			],
		);
	}
	// The server advances `main` and force-moves `v1` onto the new commit.
	std::fs::write(work.join("a.txt"), b"hello\nmore\n").unwrap();
	git(&work, &["commit", "-qam", "second"]);
	git(&work, &["tag", "-f", "v1", "main"]);
	git(
		&work,
		&[
			"push",
			"-q",
			"--force",
			bare.to_str().unwrap(),
			"main",
			"v1",
		],
	);

	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"git tag fetch",
	);
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"gta tag fetch",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	assert_eq!(
		last_line(&gta_dir, "refs/tags/v1"),
		last_line(&git_dir, "refs/tags/v1"),
		"refs/tags/v1 reflog after a forced tag update"
	);
	assert!(
		last_line(&gta_dir, "refs/tags/v1").ends_with("fetch: updating tag"),
		"expected updating tag, got: {}",
		last_line(&gta_dir, "refs/tags/v1")
	);
}

/// `gta fetch` honours `GIT_REFLOG_ACTION` as the reflog action prefix, exactly as git does — so a
/// wrapper that sets it (a script, or `git pull` driving `git fetch`) gets its label on the entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_reflog_honours_git_reflog_action_env() {
	if skip() {
		return;
	}
	let root = tmp("reflog-action-env");
	build_bare(&root);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	// Bring gta's tracking refs up to git's (see the module note), so the update below is a like-for-like
	// fast-forward in both.
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"gta initial fetch",
	);

	// The server advances `main`.
	let work = root.join("work");
	std::fs::write(work.join("a.txt"), b"hello\nmore\n").unwrap();
	git(&work, &["commit", "-qam", "second"]);
	git(
		&work,
		&[
			"push",
			"-q",
			root.join("repo.git").to_str().unwrap(),
			"main",
		],
	);

	// Both clients fetch with a custom `GIT_REFLOG_ACTION`; both must stamp it on the tracking reflog.
	let action = vec![("GIT_REFLOG_ACTION".into(), "sync job".into())];
	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			action.clone(),
		)
		.await,
		"git fetch (custom action)",
	);
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			action,
		)
		.await,
		"gta fetch (custom action)",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/main"),
		last_line(&git_dir, "refs/remotes/origin/main"),
		"origin/main reflog under a custom GIT_REFLOG_ACTION"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/main").ends_with("sync job: fast-forward"),
		"expected the custom action, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/main")
	);

	// An *explicitly empty* `GIT_REFLOG_ACTION` is still "set" for git — it records `: <status>` (empty
	// action), not the default `fetch`. gta must match rather than fall back.
	std::fs::write(work.join("a.txt"), b"hello\nmore\nyet more\n").unwrap();
	git(&work, &["commit", "-qam", "third"]);
	git(
		&work,
		&[
			"push",
			"-q",
			root.join("repo.git").to_str().unwrap(),
			"main",
		],
	);
	let empty = vec![("GIT_REFLOG_ACTION".into(), String::new())];
	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			empty.clone(),
		)
		.await,
		"git fetch (empty action)",
	);
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			empty,
		)
		.await,
		"gta fetch (empty action)",
	);
	assert_eq!(
		last_line(&gta_dir, "refs/remotes/origin/main"),
		last_line(&git_dir, "refs/remotes/origin/main"),
		"origin/main reflog under an empty GIT_REFLOG_ACTION"
	);
	assert!(
		last_line(&gta_dir, "refs/remotes/origin/main").ends_with("\t: fast-forward"),
		"expected an empty action, got: {}",
		last_line(&gta_dir, "refs/remotes/origin/main")
	);
}

/// A plain fetch auto-follows a new tag; with tag reflogs enabled (`core.logAllRefUpdates=always`) git
/// records `fetch: storing tag` on `logs/refs/tags/<tag>`, and gta matches. (By default that namespace
/// is unlogged, so nothing is written — also matching git.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_followed_tag_reflog_matches_git() {
	if skip() {
		return;
	}
	let root = tmp("autotag-reflog");
	build_bare(&root);
	let base = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{base}/repo.git");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);
	for co in [&git_co, &gta_co] {
		git(co, &["config", "core.logAllRefUpdates", "always"]);
	}
	// The server gains a new annotated tag reachable from `main`, which a plain fetch auto-follows.
	let work = root.join("work");
	git(&work, &["tag", "-a", "v9", "-m", "v9"]);
	git(
		&work,
		&["push", "-q", root.join("repo.git").to_str().unwrap(), "v9"],
	);
	ok(
		&git_client(
			vec!["-C".into(), git_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"git fetch",
	);
	ok(
		&gta_client(
			vec!["-C".into(), gta_co.to_str().unwrap().into(), "fetch".into()],
			vec![],
		)
		.await,
		"gta fetch",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	assert_eq!(
		reflog(&gta_dir, "refs/tags/v9"),
		reflog(&git_dir, "refs/tags/v9"),
		"auto-followed tag reflog"
	);
	assert!(
		last_line(&gta_dir, "refs/tags/v9").ends_with("fetch: storing tag"),
		"expected storing tag, got: {}",
		last_line(&gta_dir, "refs/tags/v9")
	);
}

/// git records the clone source URL verbatim — a trailing slash and all — not `Origin`'s normalized
/// form. gta uses the URL as typed for the `clone: from <url>` reflog, so it matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_reflog_preserves_url_text() {
	if skip() {
		return;
	}
	let root = tmp("clone-url-reflog");
	build_bare(&root);
	let base = serve_git_http_backend(root.clone()).await;
	// A trailing slash that `Origin::parse` trims but git (and now gta) keeps in the reflog.
	let repo_url = format!("{base}/repo.git/");

	let git_co = root.join("git-clone");
	let gta_co = root.join("gta-clone");
	ok(
		&git_client(
			vec![
				"clone".into(),
				"-q".into(),
				repo_url.clone(),
				git_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"git clone",
	);
	ok(
		&gta_client(
			vec![
				"clone".into(),
				repo_url.clone(),
				gta_co.to_str().unwrap().into(),
			],
			vec![],
		)
		.await,
		"gta clone",
	);

	let git_dir = git_co.join(".git");
	let gta_dir = gta_co.join(".git");
	assert_eq!(
		reflog(&gta_dir, "HEAD"),
		reflog(&git_dir, "HEAD"),
		"logs/HEAD with a trailing-slash clone URL"
	);
	assert!(
		last_line(&gta_dir, "HEAD").ends_with(&format!("clone: from {repo_url}")),
		"expected the verbatim URL, got: {}",
		last_line(&gta_dir, "HEAD")
	);
}
