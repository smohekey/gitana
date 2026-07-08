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

/// Fetching when already up to date still succeeds: git sends a valid empty (0-object) packfile, which
/// must not be mistaken for the empty-body server-error case (which `fetch_pack` now rejects).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetch_when_up_to_date_succeeds() {
	if skip() {
		return;
	}
	let root = tmp("client-uptodate");
	build_bare(&root);
	let url = serve_git_http_backend(root.clone()).await;
	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", &format!("{url}/repo.git"), c]).await,
		"clone",
	);
	// No server change: a second fetch downloads a valid 0-object pack and must still succeed.
	gta_ok(&gta(&["-C", c, "fetch"]).await, "up-to-date fetch");
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

/// Build `<root>/repo.git`: a bare git repo with a linear history of `n` commits on `main`
/// (`a.txt` = `v0`, `v1`, …), HTTP push enabled. Returns the commit ids oldest→newest; the source
/// work tree stays at `<root>/work`.
fn build_linear_bare(root: &Path, n: usize) -> Vec<String> {
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	let mut ids = Vec::new();
	for i in 0..n {
		std::fs::write(work.join("a.txt"), format!("v{i}\n")).unwrap();
		git(&work, &["add", "."]);
		git(&work, &["commit", "-qm", &format!("c{i}")]);
		ids.push(git(&work, &["rev-parse", "HEAD"]));
	}
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
	ids
}

/// The sorted commit ids in a checkout's `.git/shallow` (empty when the file is absent).
fn shallow_set(checkout: &Path) -> Vec<String> {
	match std::fs::read_to_string(checkout.join(".git/shallow")) {
		Ok(text) => {
			let mut ids: Vec<String> = text
				.lines()
				.map(|line| line.trim().to_owned())
				.filter(|line| !line.is_empty())
				.collect();
			ids.sort();
			ids
		}
		Err(_) => Vec::new(),
	}
}

/// `gta clone --depth N` against a real `git http-backend` truncates history exactly as stock git's own
/// `--depth N` clone does: the same `.git/shallow` boundary, and the objects past it genuinely absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_clones_a_real_git_repo() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow");
	let ids = build_linear_bare(&root, 3); // c0 (root) <- c1 <- c2 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");
	let tip = ids[2].clone();
	let parent = ids[1].clone();
	let grandparent = ids[0].clone();

	// --depth 1: only the tip commit; its parent is the boundary's cut point.
	let gta1 = root.join("gta1");
	let g1 = gta1.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &repo_url, g1]).await,
		"shallow clone",
	);
	// HEAD is the server tip, checked out.
	assert_eq!(
		gta_stdout(&gta(&["-C", g1, "rev-parse", "HEAD"]).await, "rev-parse"),
		tip
	);
	// The boundary matches git's own --depth 1 clone (which is exactly the tip), and the parent is gone.
	let ref1 = root.join("git1");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--depth",
			"1",
			"-q",
			&repo_url,
			ref1.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"git shallow clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		shallow_set(&gta1),
		shallow_set(&ref1),
		"gta and git disagree on the shallow boundary"
	);
	assert_eq!(shallow_set(&gta1), vec![tip.clone()]);
	assert!(
		!gta(&["-C", g1, "cat-file", "-t", &parent])
			.await
			.status
			.success(),
		"the parent past the shallow boundary must be absent"
	);
	// rev-list on the shallow clone stops at the boundary rather than chasing the absent parent.
	assert_eq!(
		gta_stdout(&gta(&["-C", g1, "rev-list", "HEAD"]).await, "rev-list"),
		tip
	);

	// --depth 2: tip + parent present; the root (grandparent) past the boundary is absent; boundary = parent.
	let gta2 = root.join("gta2");
	let g2 = gta2.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "2", &repo_url, g2]).await,
		"depth-2 clone",
	);
	let ref2 = root.join("git2");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--depth",
			"2",
			"-q",
			&repo_url,
			ref2.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "git depth-2 clone failed");
	assert_eq!(shallow_set(&gta2), shallow_set(&ref2));
	assert_eq!(shallow_set(&gta2), vec![parent.clone()]);
	assert!(
		gta(&["-C", g2, "cat-file", "-t", &parent])
			.await
			.status
			.success(),
		"the parent is present at depth 2"
	);
	assert!(
		!gta(&["-C", g2, "cat-file", "-t", &grandparent])
			.await
			.status
			.success(),
		"the root past the depth-2 boundary is absent"
	);
}

/// A shallow clone deepens only from branch tips, matching git: an annotated tag whose target is
/// *within* the requested depth is kept (via `include-tag`), while a tag on history *outside* the depth
/// is neither fetched nor recreated — so `--depth`/`--shallow-exclude` are not defeated by an old tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_clone_keeps_reachable_tags_prunes_the_rest() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow-tag");
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	std::fs::write(work.join("a.txt"), b"v0\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c0"]);
	git(&work, &["tag", "-a", "oldtag", "-m", "old"]); // annotated tag on the root, outside depth 1
	let old = git(&work, &["rev-parse", "oldtag"]);
	std::fs::write(work.join("a.txt"), b"v1\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c1"]);
	std::fs::write(work.join("a.txt"), b"v2\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c2"]);
	git(&work, &["tag", "-a", "newtag", "-m", "new"]); // annotated tag on the tip, within depth 1
	let tip = git(&work, &["rev-parse", "HEAD"]);
	let newtag = git(&work, &["rev-parse", "newtag"]);
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
	assert!(out.status.success(), "bare clone failed");

	let url = serve_git_http_backend(root.clone()).await;
	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &format!("{url}/repo.git"), c]).await,
		"shallow clone",
	);
	// The shallow boundary is the tip.
	assert_eq!(shallow_set(&checkout), vec![tip]);
	// The annotated tag pointing at the tip (within depth) is preserved at its tag-object id.
	assert_eq!(
		gta_stdout(
			&gta(&["-C", c, "rev-parse", "newtag"]).await,
			"rev-parse newtag"
		),
		newtag
	);
	// The tag on the pruned root is neither recreated nor fetched.
	assert!(
		!gta(&["-C", c, "rev-parse", "oldtag"])
			.await
			.status
			.success(),
		"a tag outside the shallow depth must not be recreated"
	);
	assert!(
		!gta(&["-C", c, "cat-file", "-t", &old])
			.await
			.status
			.success(),
		"the tagged object outside the shallow depth must be absent"
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

/// `gta fetch --unshallow` fills in the complete history of a shallow clone, matching stock git: the
/// `.git/shallow` boundary is dropped and every ancestor past it becomes present. A second `--unshallow`
/// on the now-complete repo is refused, as git refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetch_unshallow_fills_history() {
	if skip() {
		return;
	}
	let root = tmp("client-unshallow");
	let ids = build_linear_bare(&root, 3); // c0 (root) <- c1 <- c2 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");
	let tip = ids[2].clone();
	let parent = ids[1].clone();
	let grandparent = ids[0].clone();

	let gta_dir = root.join("c");
	let c = gta_dir.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &repo_url, c]).await,
		"shallow clone",
	);
	assert_eq!(shallow_set(&gta_dir), vec![tip.clone()]);
	assert!(
		!gta(&["-C", c, "cat-file", "-t", &parent])
			.await
			.status
			.success(),
		"precondition: the parent is absent in the shallow clone"
	);

	gta_ok(
		&gta(&["-C", c, "fetch", "--unshallow"]).await,
		"fetch --unshallow",
	);
	// The repo is no longer shallow (git's `fetch --unshallow` likewise removes `.git/shallow`).
	assert!(
		shallow_set(&gta_dir).is_empty(),
		"the repo must no longer be shallow after --unshallow"
	);
	// Every ancestor past the old boundary is now present, and rev-list walks the full history.
	assert!(
		gta(&["-C", c, "cat-file", "-t", &parent])
			.await
			.status
			.success(),
		"the parent is present after --unshallow"
	);
	assert!(
		gta(&["-C", c, "cat-file", "-t", &grandparent])
			.await
			.status
			.success(),
		"the root is present after --unshallow"
	);
	let rev_list = gta_stdout(&gta(&["-C", c, "rev-list", "HEAD"]).await, "rev-list");
	assert_eq!(rev_list.lines().count(), 3, "the full history is walkable");

	// `--unshallow` on a complete repository does not make sense; git rejects it, and so do we.
	assert!(
		!gta(&["-C", c, "fetch", "--unshallow"])
			.await
			.status
			.success(),
		"--unshallow of a complete repo must be refused"
	);
}

/// `gta fetch --deepen N` extends a shallow clone's boundary by N commits from its current frontier,
/// landing on the same `.git/shallow` boundary as stock git's own `fetch --deepen N`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetch_deepen_advances_the_boundary() {
	if skip() {
		return;
	}
	let root = tmp("client-deepen");
	let ids = build_linear_bare(&root, 3); // c0 (root) <- c1 <- c2 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");
	let tip = ids[2].clone();
	let parent = ids[1].clone();
	let grandparent = ids[0].clone();

	// gta: a depth-1 clone, then deepen by one commit.
	let gta_dir = root.join("gta");
	let g = gta_dir.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &repo_url, g]).await,
		"shallow clone",
	);
	assert_eq!(shallow_set(&gta_dir), vec![tip.clone()]);
	gta_ok(
		&gta(&["-C", g, "fetch", "--deepen", "1"]).await,
		"fetch --deepen 1",
	);

	// git oracle: the same depth-1 clone + `fetch --deepen 1`.
	let ref_dir = root.join("git");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--depth",
			"1",
			"-q",
			&repo_url,
			ref_dir.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"git shallow clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	let out = git_try(&ref_dir, &["fetch", "--deepen", "1", "-q"]);
	assert!(
		out.status.success(),
		"git fetch --deepen failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	// The boundary advanced to the parent, matching git; the parent is present, the root still absent.
	assert_eq!(
		shallow_set(&gta_dir),
		shallow_set(&ref_dir),
		"gta and git disagree on the deepened boundary"
	);
	assert_eq!(shallow_set(&gta_dir), vec![parent.clone()]);
	assert!(
		gta(&["-C", g, "cat-file", "-t", &parent])
			.await
			.status
			.success(),
		"the parent is present after --deepen 1"
	);
	assert!(
		!gta(&["-C", g, "cat-file", "-t", &grandparent])
			.await
			.status
			.success(),
		"the root is still past the deepened boundary"
	);
}

/// `gta fetch --depth 1` on a *full* clone truncates local history: it records the same `.git/shallow`
/// boundary as stock git's `fetch --depth 1`, and a subsequent rev-list stops at the tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_fetch_depth_truncates_a_full_clone() {
	if skip() {
		return;
	}
	let root = tmp("client-fetch-depth");
	let ids = build_linear_bare(&root, 3); // c0 (root) <- c1 <- c2 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");
	let tip = ids[2].clone();

	let gta_dir = root.join("gta");
	let g = gta_dir.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, g]).await, "full clone");
	assert!(
		shallow_set(&gta_dir).is_empty(),
		"a full clone is not shallow"
	);
	gta_ok(
		&gta(&["-C", g, "fetch", "--depth", "1"]).await,
		"fetch --depth 1",
	);

	// git oracle: a full clone + `fetch --depth 1`.
	let ref_dir = root.join("git");
	let out = git_try(
		Path::new("."),
		&["clone", "-q", &repo_url, ref_dir.to_str().unwrap()],
	);
	assert!(out.status.success(), "git full clone failed");
	let out = git_try(&ref_dir, &["fetch", "--depth", "1", "-q"]);
	assert!(
		out.status.success(),
		"git fetch --depth failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	assert_eq!(
		shallow_set(&gta_dir),
		shallow_set(&ref_dir),
		"gta and git disagree on the truncated boundary"
	);
	assert_eq!(shallow_set(&gta_dir), vec![tip.clone()]);
	// The truncation is effective: rev-list stops at the tip rather than walking the retained ancestors.
	assert_eq!(
		gta_stdout(&gta(&["-C", g, "rev-list", "HEAD"]).await, "rev-list"),
		tip
	);
}

/// Shallow-fetch tag handling, matching git: an auto-follow fetch does *not* pull a tag whose object is
/// outside the boundary (like `oldtag` on the root at `--depth 1`) — it leaves it alone rather than
/// choke reading the absent object — while `--tags` fetches that same tag as its own shallow root,
/// landing the tag ref and extending `.git/shallow` exactly as stock git's `fetch --depth 1 --tags`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_fetch_tags_follow_git() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow-fetch-tag");
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	std::fs::write(work.join("a.txt"), b"root\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c0"]);
	git(&work, &["tag", "-a", "oldtag", "-m", "old"]); // annotated tag on the root, outside depth 1
	let oldtag = git(&work, &["rev-parse", "oldtag"]);
	std::fs::write(work.join("a.txt"), b"tip\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c1"]);
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
	assert!(out.status.success(), "bare clone failed");
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");

	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &repo_url, c]).await,
		"shallow clone",
	);
	// The out-of-boundary tag was not recreated at clone (its object is absent).
	assert!(
		!gta(&["-C", c, "rev-parse", "refs/tags/oldtag"])
			.await
			.status
			.success(),
		"precondition: the out-of-boundary tag is absent after a shallow clone"
	);

	// A shallow auto-follow fetch must not fail peeling the still-absent tag, and must not pull it
	// (git does not auto-follow a tag on unfetched history).
	gta_ok(
		&gta(&["-C", c, "fetch", "--depth", "1"]).await,
		"shallow auto-follow fetch",
	);
	assert!(
		!gta(&["-C", c, "rev-parse", "refs/tags/oldtag"])
			.await
			.status
			.success(),
		"an auto-follow shallow fetch leaves the out-of-boundary tag alone"
	);

	// `--tags` fetches the out-of-boundary tag as its own shallow root: the tag ref lands at the tag
	// object, and the boundary matches stock git's own `fetch --depth 1 --tags`.
	gta_ok(
		&gta(&["-C", c, "fetch", "--depth", "1", "--tags"]).await,
		"shallow fetch --tags",
	);
	assert_eq!(
		gta_stdout(
			&gta(&["-C", c, "rev-parse", "refs/tags/oldtag"]).await,
			"rev-parse oldtag"
		),
		oldtag,
		"--tags fetches the out-of-boundary tag as a shallow root"
	);

	// git oracle: a depth-1 clone + `fetch --depth 1 --tags` lands the same shallow boundary.
	let ref_dir = root.join("git");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--depth",
			"1",
			"-q",
			&repo_url,
			ref_dir.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "git shallow clone failed");
	let out = git_try(&ref_dir, &["fetch", "--depth", "1", "--tags", "-q"]);
	assert!(
		out.status.success(),
		"git fetch --tags failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		shallow_set(&checkout),
		shallow_set(&ref_dir),
		"gta and git disagree on the boundary after a shallow --tags fetch"
	);
}

/// A shallow fetch deepens only the refs its refspecs select: a branch a negative refspec
/// (`^refs/heads/large`) excludes is neither downloaded nor recorded in `.git/shallow`, even though it is
/// advertised. (Without the refspec-derived roots this would still import the excluded branch at depth 1
/// and mark the repo shallow for a branch it never tracks.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_fetch_honors_negative_refspec() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow-neg-refspec");
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	std::fs::write(work.join("a.txt"), b"c0\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c0"]);
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
	assert!(out.status.success(), "bare clone failed");
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");

	// Full clone of `main` only (the `large` branch does not exist yet).
	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");

	// The server gains a `large` branch with its own commit, then the client is configured to exclude it.
	git(&work, &["checkout", "-qb", "large"]);
	std::fs::write(work.join("b.txt"), b"large\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "e1"]);
	let large_tip = git(&work, &["rev-parse", "HEAD"]);
	git(&work, &["push", "-q", bare.to_str().unwrap(), "large"]);
	// Add a negative refspec alongside the default positive one (git honours both `fetch` lines).
	let config_path = checkout.join(".git/config");
	let config = std::fs::read_to_string(&config_path).unwrap();
	let config = config.replace(
		"fetch = +refs/heads/*:refs/remotes/origin/*",
		"fetch = +refs/heads/*:refs/remotes/origin/*\n\tfetch = ^refs/heads/large",
	);
	std::fs::write(&config_path, config).unwrap();

	// A shallow fetch must not import the excluded branch nor mark it shallow.
	gta_ok(
		&gta(&["-C", c, "fetch", "--depth", "1"]).await,
		"shallow fetch with a negative refspec",
	);
	assert!(
		!gta(&["-C", c, "cat-file", "-t", &large_tip])
			.await
			.status
			.success(),
		"the excluded branch's object must not be downloaded by a shallow fetch"
	);
	assert!(
		!shallow_set(&checkout).contains(&large_tip),
		"the excluded branch must not appear in .git/shallow"
	);
	assert!(
		!gta(&["-C", c, "rev-parse", "refs/remotes/origin/large"])
			.await
			.status
			.success(),
		"the excluded branch gets no tracking ref"
	);
}

/// `--unshallow` completes *every* existing `.git/shallow` boundary, even one for a branch a narrowed
/// refspec no longer selects — matching stock git. The client sends `shallow` lines for its whole
/// boundary (not just the selected refs) and the server unshallows all of them; the boundary commits are
/// never `want`ed directly (upload-pack would reject a non-tip want), so this needs no unsafe request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_unshallow_completes_boundaries_of_unselected_branches() {
	if skip() {
		return;
	}
	let root = tmp("client-unshallow-narrowed");
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "S"]);
	git(&work, &["config", "user.email", "s@e"]);
	std::fs::write(work.join("a.txt"), b"c0\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c0"]);
	std::fs::write(work.join("a.txt"), b"c1\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c1"]);
	git(&work, &["checkout", "-qb", "other"]);
	std::fs::write(work.join("b.txt"), b"d0\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "d0"]);
	let other_root = git(&work, &["rev-parse", "HEAD"]);
	std::fs::write(work.join("b.txt"), b"d1\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "d1"]);
	git(&work, &["checkout", "-q", "main"]);
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
	assert!(out.status.success(), "bare clone failed");
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");

	// A shallow clone truncates *both* branches at depth 1 (gta clone deepens every branch tip).
	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &repo_url, c]).await,
		"shallow clone",
	);
	assert_eq!(
		shallow_set(&checkout).len(),
		2,
		"both branch tips are shallow boundaries after a depth-1 clone"
	);
	assert!(
		!gta(&["-C", c, "cat-file", "-t", &other_root])
			.await
			.status
			.success(),
		"precondition: the other branch's parent is absent"
	);

	// Narrow the refspec to `main` only, then unshallow. The `other` branch is no longer selected.
	let config_path = checkout.join(".git/config");
	let config = std::fs::read_to_string(&config_path).unwrap();
	let config = config.replace(
		"fetch = +refs/heads/*:refs/remotes/origin/*",
		"fetch = +refs/heads/main:refs/remotes/origin/main",
	);
	std::fs::write(&config_path, config).unwrap();
	gta_ok(&gta(&["-C", c, "fetch", "--unshallow"]).await, "unshallow");

	// The repo is fully complete: no `.git/shallow`, and the unselected branch's history is present too —
	// exactly as stock git leaves it.
	assert!(
		shallow_set(&checkout).is_empty(),
		"--unshallow completes every boundary, including the unselected branch's"
	);
	assert!(
		gta(&["-C", c, "cat-file", "-t", &other_root])
			.await
			.status
			.success(),
		"the unselected branch's parent is now present"
	);
}

/// A shallow fetch normally sets git's `include-tag` so an annotated tag on newly-fetched history
/// arrives, but `--no-tags` clears it (as git does). Two depth-1 checkouts fetch the same new
/// tag-carrying commit: the `--no-tags` one gets neither the tag object nor the ref, while the default
/// one auto-follows it — proving the suppression is the flag's doing, not a re-fetch artifact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_fetch_no_tags_suppresses_include_tag() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow-no-tags");
	build_linear_bare(&root, 1); // c0 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");

	// Two depth-1 checkouts at the same tip.
	let no_tags = root.join("no-tags");
	let with_tags = root.join("with-tags");
	for dir in [&no_tags, &with_tags] {
		gta_ok(
			&gta(&["clone", "--depth", "1", &repo_url, dir.to_str().unwrap()]).await,
			"shallow clone",
		);
	}

	// The server advances `main` by one commit and puts an annotated tag on that new commit, so
	// `include-tag` has fresh pack content to attach the tag to.
	let work = root.join("work");
	std::fs::write(work.join("a.txt"), b"c1\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "c1"]);
	git(&work, &["tag", "-a", "ontip", "-m", "on the new tip"]);
	let ontip = git(&work, &["rev-parse", "ontip"]);
	git(
		&work,
		&[
			"push",
			"-q",
			root.join("repo.git").to_str().unwrap(),
			"main",
			"ontip",
		],
	);

	// `--no-tags`: the tag on the freshly-fetched commit must not be downloaded or written.
	let nt = no_tags.to_str().unwrap();
	gta_ok(
		&gta(&["-C", nt, "fetch", "--depth", "1", "--no-tags"]).await,
		"shallow fetch --no-tags",
	);
	assert!(
		!gta(&["-C", nt, "cat-file", "-t", &ontip])
			.await
			.status
			.success(),
		"--no-tags must not download the reachable tag object (no include-tag)"
	);
	assert!(
		!gta(&["-C", nt, "rev-parse", "refs/tags/ontip"])
			.await
			.status
			.success(),
		"--no-tags must not write the tag ref"
	);

	// The default (tag-following) shallow fetch of the same commit does deliver the tag.
	let wt = with_tags.to_str().unwrap();
	gta_ok(
		&gta(&["-C", wt, "fetch", "--depth", "1"]).await,
		"shallow fetch (default tags)",
	);
	assert_eq!(
		gta_stdout(
			&gta(&["-C", wt, "rev-parse", "refs/tags/ontip"]).await,
			"rev-parse ontip"
		),
		ontip,
		"a default shallow fetch delivers the tag on the fetched commit"
	);
}

/// A shallow fetch that fails its refspec validation must not leave the repository truncated: the
/// `.git/shallow` boundary is written inside the download, so the fatal checks run *before* it. A
/// configured refspec naming a ref the remote does not advertise fails the fetch, and `.git/shallow`
/// stays absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_failed_shallow_fetch_leaves_no_shallow_state() {
	if skip() {
		return;
	}
	let root = tmp("client-shallow-fail");
	build_linear_bare(&root, 2); // c0 <- c1 (tip)
	let url = serve_git_http_backend(root.clone()).await;
	let repo_url = format!("{url}/repo.git");

	// A full clone (not shallow).
	let checkout = root.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(&gta(&["clone", &repo_url, c]).await, "clone");
	assert!(
		shallow_set(&checkout).is_empty(),
		"a full clone is not shallow"
	);

	// Add a fetch refspec whose exact source the remote does not advertise — a fatal `couldn't find
	// remote ref` error.
	let config_path = checkout.join(".git/config");
	let config = std::fs::read_to_string(&config_path).unwrap();
	let config = config.replace(
		"fetch = +refs/heads/*:refs/remotes/origin/*",
		"fetch = +refs/heads/*:refs/remotes/origin/*\n\tfetch = \
		 +refs/heads/nonexistent:refs/remotes/origin/nonexistent",
	);
	std::fs::write(&config_path, config).unwrap();

	// The shallow fetch fails on the bad refspec — and must not have written a shallow boundary first.
	assert!(
		!gta(&["-C", c, "fetch", "--depth", "1"])
			.await
			.status
			.success(),
		"the fetch must fail on the unadvertised refspec source"
	);
	assert!(
		shallow_set(&checkout).is_empty(),
		"a failed shallow fetch must not persist a .git/shallow boundary"
	);
}
