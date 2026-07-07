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
