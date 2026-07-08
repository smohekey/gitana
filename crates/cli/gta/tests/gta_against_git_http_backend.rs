//! Direction B of the remote-interop suite: **`gta` as client, a real `git` server.** A `git
//! http-backend` CGI program is bridged behind axum ([`support::serve_git_http_backend`]); the gitana
//! `gta` client clones/fetches/pushes against a real bare git repo over that. Proves gitana's
//! Smart-HTTP client interoperates with stock git — the reverse of Direction A.
//!
//! gitana's client speaks protocol v0, which `git http-backend` serves. SHA-1 (git's default). Gated
//! on `git http-backend` being installed.

mod support;

use std::path::Path;

use support::{
	git, git_http_backend_available, git_try, gta, gta_ok, gta_stdout, serve_git_http_backend,
};

/// Build `<root>/repo.git`: a bare git repo with one commit on `main` (`a.txt` = `hello\n`), a `dev`
/// branch, a lightweight tag `lw`, and an annotated tag `v1`, with HTTP push enabled. Returns the
/// `main` tip and the annotated tag's id (hex). The source work tree stays at `<root>/work` so tests
/// can advance the server.
fn build_bare(root: &Path) -> (String, String) {
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "first"]);
	git(&work, &["branch", "dev"]);
	git(&work, &["tag", "lw"]);
	git(&work, &["tag", "-a", "v1", "-m", "release"]);
	let head = git(&work, &["rev-parse", "HEAD"]);
	let tag_id = git(&work, &["rev-parse", "v1"]);

	let bare = root.join("repo.git");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--bare",
			"-q",
			work.to_str().unwrap(),
			bare.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"bare clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// http-backend refuses receive-pack over HTTP unless the served repo opts in.
	git(&bare, &["config", "http.receivepack", "true"]);
	(head, tag_id)
}

/// A fresh temp dir (this file doesn't share Direction A's `unique_tmp` import to keep it minimal).
fn tmp(tag: &str) -> std::path::PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn skip() -> bool {
	if !git_http_backend_available() {
		eprintln!("skipping: git http-backend not available");
		return true;
	}
	false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_clones_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-clone");
	let (head, tag_id) = build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	gta_ok(
		&gta(&["clone", &repo_url, checkout.to_str().unwrap()]).await,
		"clone",
	);

	// HEAD checked out to the server's main tip, with the file content; both tags landed.
	assert_eq!(
		gta_stdout(
			&gta(&["-C", checkout.to_str().unwrap(), "rev-parse", "HEAD"]).await,
			"rev-parse"
		),
		head
	);
	assert_eq!(std::fs::read(checkout.join("a.txt")).unwrap(), b"hello\n");
	assert_eq!(
		gta_stdout(
			&gta(&["-C", checkout.to_str().unwrap(), "rev-parse", "v1"]).await,
			"rev-parse v1"
		),
		tag_id
	);
	assert_eq!(
		gta_stdout(
			&gta(&["-C", checkout.to_str().unwrap(), "rev-parse", "lw"]).await,
			"rev-parse lw"
		),
		head
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetches_from_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-fetch");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");

	// The real server advances `main`; gta fetch follows it into the tracking ref.
	let work = root.join("work");
	std::fs::write(work.join("b.txt"), b"more\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "second"]);
	git(
		&work,
		&[
			"push",
			"-q",
			root.join("repo.git").to_str().unwrap(),
			"main",
		],
	);
	let advanced = git(&work, &["rev-parse", "HEAD"]);

	gta_ok(&gta(&["-C", c, "fetch"]).await, "fetch");
	// The full tracking ref (gta's rev-parse does not DWIM the `origin/main` shorthand to
	// `refs/remotes/origin/main` the way git does — a separate rev-parse gap, not a fetch bug).
	assert_eq!(
		gta_stdout(
			&gta(&["-C", c, "rev-parse", "refs/remotes/origin/main"]).await,
			"rev-parse tracking ref"
		),
		advanced
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetches_tags_from_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-fetch-tags");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");

	// The real server gains a NEW annotated tag after the clone (its advertisement carries a
	// `refs/tags/v2^{}` peel line — gta must fetch the tag ref, not a junk `^{}` ref).
	let work = root.join("work");
	git(&work, &["tag", "-a", "v2", "-m", "another"]);
	let v2 = git(&work, &["rev-parse", "v2"]);
	git(
		&work,
		&["push", "-q", root.join("repo.git").to_str().unwrap(), "v2"],
	);

	// A plain fetch auto-follows the new tag: its target is reachable from the fetched `main`, so gta
	// lands `refs/tags/v2` at the tag object id (not the peeled commit — the advertisement's
	// `refs/tags/v2^{}` peel line is dropped, not written as a junk ref).
	gta_ok(&gta(&["-C", c, "fetch"]).await, "fetch");
	assert_eq!(
		gta_stdout(
			&gta(&["-C", c, "rev-parse", "refs/tags/v2"]).await,
			"rev-parse v2"
		),
		v2
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_pushes_to_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-push");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");

	// A local commit through gta, pushed to the real bare repo.
	gta_ok(
		&gta(&["-C", c, "config", "user.name", "C"]).await,
		"config name",
	);
	gta_ok(
		&gta(&["-C", c, "config", "user.email", "c@e"]).await,
		"config email",
	);
	std::fs::write(checkout.join("a.txt"), b"changed\n").unwrap();
	gta_ok(&gta(&["-C", c, "add", "."]).await, "add");
	gta_ok(
		&gta(&["-C", c, "commit", "-m", "client change"]).await,
		"commit",
	);
	let head = gta_stdout(&gta(&["-C", c, "rev-parse", "HEAD"]).await, "rev-parse");
	gta_ok(&gta(&["-C", c, "push"]).await, "push");

	// The real repo's `main` moved to the gta-authored commit.
	let bare = root.join("repo.git");
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_pushes_explicit_refspecs_to_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-refspec");
	let (head, _) = build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;
	let bare = root.join("repo.git");

	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", &format!("{url}/repo.git"), c]).await,
		"clone",
	);

	// Create a new remote branch from HEAD via an explicit `HEAD:refs/heads/x` refspec — with no
	// `origin` argument (gitana's only remote is origin, so a refspec-only push must work).
	gta_ok(
		&gta(&["-C", c, "push", "HEAD:refs/heads/feature"]).await,
		"push HEAD refspec",
	);
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/feature"]), head);

	// A `<src>:<dst>` rename push — the local `dev` (recreated by clone) to a new remote `release`.
	gta_ok(
		&gta(&["-C", c, "push", "origin", "dev:release"]).await,
		"push rename",
	);
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/release"]), head);

	// Bare `HEAD` (git's `push origin HEAD` shorthand) pushes the current branch (`main`), not a
	// literal `refs/heads/HEAD`.
	gta_ok(&gta(&["-C", c, "config", "user.name", "C"]).await, "config");
	gta_ok(
		&gta(&["-C", c, "config", "user.email", "c@e"]).await,
		"config",
	);
	std::fs::write(checkout.join("a.txt"), b"advance\n").unwrap();
	gta_ok(&gta(&["-C", c, "add", "."]).await, "add");
	gta_ok(&gta(&["-C", c, "commit", "-m", "advance"]).await, "commit");
	let advanced = gta_stdout(&gta(&["-C", c, "rev-parse", "HEAD"]).await, "rev-parse");
	gta_ok(
		&gta(&["-C", c, "push", "origin", "HEAD"]).await,
		"push bare HEAD",
	);
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), advanced);
	assert!(
		!git_try(&bare, &["rev-parse", "--verify", "refs/heads/HEAD"])
			.status
			.success(),
		"a literal refs/heads/HEAD must not be created"
	);

	// Delete a remote ref two ways: a `:<dst>` refspec, and the `--delete` flag.
	gta_ok(
		&gta(&["-C", c, "push", "origin", ":feature"]).await,
		"push delete refspec",
	);
	assert!(
		!git_try(&bare, &["rev-parse", "--verify", "refs/heads/feature"])
			.status
			.success()
	);
	gta_ok(
		&gta(&["-C", c, "push", "origin", "--delete", "release"]).await,
		"push delete flag",
	);
	assert!(
		!git_try(&bare, &["rev-parse", "--verify", "refs/heads/release"])
			.status
			.success()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_pushes_follow_tags_to_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-follow-tags");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;
	let bare = root.join("repo.git");

	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", &format!("{url}/repo.git"), c]).await,
		"clone",
	);
	gta_ok(
		&gta(&["-C", c, "config", "user.name", "C"]).await,
		"config name",
	);
	gta_ok(
		&gta(&["-C", c, "config", "user.email", "c@e"]).await,
		"config email",
	);

	// Advance `main` locally, then tag the new tip both ways.
	std::fs::write(checkout.join("a.txt"), b"changed\n").unwrap();
	gta_ok(&gta(&["-C", c, "add", "."]).await, "add");
	gta_ok(
		&gta(&["-C", c, "commit", "-m", "client change"]).await,
		"commit",
	);
	let head = gta_stdout(
		&gta(&["-C", c, "rev-parse", "HEAD"]).await,
		"rev-parse HEAD",
	);
	gta_ok(
		&gta(&["-C", c, "tag", "-a", "v2", "-m", "release"]).await,
		"tag v2",
	);
	gta_ok(&gta(&["-C", c, "tag", "v2lw"]).await, "tag v2lw");
	let v2 = gta_stdout(&gta(&["-C", c, "rev-parse", "v2"]).await, "rev-parse v2");

	// `--follow-tags` pushes `main` plus the reachable annotated tag `v2`, into the real git repo — but
	// not the lightweight `v2lw`. Real git accepts the pack (the tag's commit rides along).
	gta_ok(
		&gta(&["-C", c, "push", "--follow-tags"]).await,
		"push --follow-tags",
	);
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
	assert_eq!(git(&bare, &["rev-parse", "v2"]), v2);
	assert!(
		!git_try(&bare, &["rev-parse", "--verify", "refs/tags/v2lw"])
			.status
			.success(),
		"a lightweight tag must not be followed"
	);
}

/// A large, delta-friendly blob tagged by `marker`; a one-byte-different successor deltas well
/// against it, so a thin pack is produced when one side already has the other version.
fn big(marker: u8) -> Vec<u8> {
	let mut data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
	data.push(marker);
	data
}

/// Commit `big.txt` = `content` in the server's work tree and push it to the bare repo.
fn server_push_big(root: &Path, content: &[u8], msg: &str) -> String {
	let work = root.join("work");
	std::fs::write(work.join("big.txt"), content).unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", msg]);
	git(
		&work,
		&[
			"push",
			"-q",
			root.join("repo.git").to_str().unwrap(),
			"main",
		],
	);
	git(&work, &["rev-parse", "HEAD"])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetches_a_thin_pack_from_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-thin-fetch");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");

	// Seed a big file on the server and fetch it, so gta holds the base version.
	server_push_big(&root, &big(b'A'), "add big");
	gta_ok(&gta(&["-C", c, "fetch"]).await, "fetch base");

	// Change the big file by one byte and fetch again: git http-backend serves a thin pack (gta
	// negotiated `thin-pack` and reported the base version as a `have`); gta must complete it.
	let advanced = server_push_big(&root, &big(b'B'), "tweak big");
	gta_ok(&gta(&["-C", c, "fetch"]).await, "thin fetch");
	assert_eq!(
		gta_stdout(
			&gta(&["-C", c, "rev-parse", "refs/remotes/origin/main"]).await,
			"rev-parse tracking ref"
		),
		advanced
	);

	// The de-thinned blob is stored byte-for-byte: check it out and read it back.
	gta_ok(
		&gta(&["-C", c, "reset", "--hard", &advanced]).await,
		"reset",
	);
	assert_eq!(std::fs::read(checkout.join("big.txt")).unwrap(), big(b'B'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_pushes_a_thin_pack_to_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-thin-push");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;
	let bare = root.join("repo.git");

	let checkout = root.join("c");
	let repo_url = format!("{url}/repo.git");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");
	gta_ok(&gta(&["-C", c, "config", "user.name", "C"]).await, "config");
	gta_ok(
		&gta(&["-C", c, "config", "user.email", "c@e"]).await,
		"config",
	);

	// Bring the base version onto both sides: the server has big('A') and gta holds it locally.
	let base = server_push_big(&root, &big(b'A'), "add big");
	gta_ok(&gta(&["-C", c, "fetch"]).await, "fetch base");
	gta_ok(
		&gta(&["-C", c, "reset", "--hard", &base]).await,
		"reset to base",
	);

	// Change the big file by one byte and push: gta sends a thin pack (new blob as a REF delta
	// against the base the remote advertises); real git's receive-pack must complete it.
	std::fs::write(checkout.join("big.txt"), big(b'B')).unwrap();
	gta_ok(&gta(&["-C", c, "add", "."]).await, "add");
	gta_ok(
		&gta(&["-C", c, "commit", "-m", "tweak big"]).await,
		"commit",
	);
	let head = gta_stdout(&gta(&["-C", c, "rev-parse", "HEAD"]).await, "rev-parse");
	gta_ok(&gta(&["-C", c, "push"]).await, "thin push");

	// The ref moved, the repo is intact, and the delta-completed blob checks out byte-for-byte.
	assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"]), head);
	let fsck = git_try(&bare, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after a thin push failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
	assert_eq!(
		git(&bare, &["cat-file", "-p", &format!("{head}:big.txt")]),
		String::from_utf8(big(b'B')).unwrap()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_deletes_a_remote_tag_by_bare_name() {
	if skip() {
		return;
	}
	let root = tmp("client-delete-tag");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;
	let bare = root.join("repo.git");

	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", &format!("{url}/repo.git"), c]).await,
		"clone",
	);

	// The bare repo has the annotated tag `v1` (and no branch `v1`). Deleting the bare name resolves
	// against the remote's refs, so it removes `refs/tags/v1` rather than a nonexistent branch.
	assert!(
		git_try(&bare, &["rev-parse", "--verify", "refs/tags/v1"])
			.status
			.success(),
		"precondition: the remote has the tag"
	);
	gta_ok(
		&gta(&["-C", c, "push", "origin", "--delete", "v1"]).await,
		"push --delete v1",
	);
	assert!(
		!git_try(&bare, &["rev-parse", "--verify", "refs/tags/v1"])
			.status
			.success(),
		"the remote tag was deleted"
	);
}
