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
