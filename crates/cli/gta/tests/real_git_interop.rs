//! Direction A of the remote-interop suite: **gitana as server, stock `git` as client.** A loopback
//! axum server ([`support::serve_gitana`]) serves a gitana repo's Smart-HTTP handlers with a v0/v2
//! dispatcher; real `git` clones/fetches/pushes against it. Every fetch-side case runs under both
//! forced `-c protocol.version=2` (git's default, and gitana's never-real-tested `ls-refs`/`fetch`
//! path) and forced `-c protocol.version=0`, so a v2-only failure is isolated as a real conformance
//! bug.
//!
//! SHA-1 (git's default) unless a case is explicitly SHA-256 (gated on `git_supports_sha256`).

mod support;

use std::path::Path;

use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Sha1, Sha256, Tag, encode_tag};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use support::{ServerHash, git, git_supports_sha256, git_try, open, serve_gitana, unique_tmp};

/// A fixed identity for the server's commits/tags (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

/// Record a commit of `file` = `content` on the repo's `HEAD` branch; returns the commit id.
async fn commit_on<H: HashAlgorithm>(
	repo: &Repository<gitana_file_store_local::LocalFileStore, H>,
	file: &str,
	content: &[u8],
) -> ObjectId<H> {
	let blob = repo.write_blob(content).await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: file.to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	repo.commit_on_head(tree, WHO, WHO, "srv\n").await.unwrap()
}

/// Build a server repo (hash `H`) at `git_dir`: one commit on `main` (`a.txt` = `hello\n`), a `dev`
/// branch at that commit, a lightweight tag `lw`, and an annotated tag `v1`. Returns the main tip and
/// the annotated tag's own id.
async fn build_server<H: HashAlgorithm>(git_dir: &Path) -> (ObjectId<H>, ObjectId<H>) {
	std::fs::create_dir_all(git_dir).unwrap();
	let repo = open::<H>(git_dir);
	repo.init().await.unwrap();
	let head = commit_on(&repo, "a.txt", b"hello\n").await;

	repo
		.refs()
		.update_ref("refs/heads/dev", head, None)
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/tags/lw", head, None)
		.await
		.unwrap();

	let tag = Tag::<H> {
		object: head,
		kind: ObjectKind::Commit,
		name: "v1".to_owned(),
		tagger: Some(WHO.to_owned()),
		signature: None,
		message: "release\n".to_owned(),
	};
	let tag_id = repo
		.objects()
		.write_object(ObjectKind::Tag, &encode_tag(&tag))
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/tags/v1", tag_id, None)
		.await
		.unwrap();
	(head, tag_id)
}

/// `git -c protocol.version=<version> clone`. Forcing the version (rather than relying on git's
/// default) guarantees the intended wire protocol is exercised. Returns the raw clone output.
fn clone(url: &str, into: &Path, version: u8) -> std::process::Output {
	git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			url,
			into.to_str().unwrap(),
		],
	)
}

fn assert_cloned(out: &std::process::Output, version: u8) {
	assert!(
		out.status.success(),
		"git clone (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

// --- clone --------------------------------------------------------------------------------------

async fn clone_case(version: u8) {
	let work = unique_tmp("interop-clone");
	let (head, _) = build_server::<Sha1>(&work.join("srv.git")).await;
	let url = serve_gitana(work.join("srv.git"), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, version), version);

	// HEAD checked out to the server's main tip, with the file content.
	assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head.to_hex());
	assert_eq!(std::fs::read(checkout.join("a.txt")).unwrap(), b"hello\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_clones_gitana_over_v2() {
	clone_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_clones_gitana_over_v0() {
	clone_case(0).await;
}

// --- fetch --------------------------------------------------------------------------------------

async fn fetch_case(version: u8) {
	let work = unique_tmp("interop-fetch");
	let git_dir = work.join("srv.git");
	build_server::<Sha1>(&git_dir).await;
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, version), version);

	// The server advances `main`; the client fetches and its remote-tracking ref follows.
	let advanced = commit_on(&open::<Sha1>(&git_dir), "b.txt", b"more\n").await;
	let out = git_try(
		&checkout,
		&[
			"-c",
			&format!("protocol.version={version}"),
			"fetch",
			"origin",
		],
	);
	assert!(
		out.status.success(),
		"git fetch (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		git(&checkout, &["rev-parse", "origin/main"]),
		advanced.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_gitana_over_v2() {
	fetch_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_gitana_over_v0() {
	fetch_case(0).await;
}

// --- push (v0 on the wire regardless; regression coverage of the dispatcher) ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_pushes_into_gitana() {
	let work = unique_tmp("interop-push");
	let git_dir = work.join("srv.git");
	build_server::<Sha1>(&git_dir).await;
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, 2), 2);

	// A local commit through stock git, pushed back to gitana.
	git(&checkout, &["config", "user.name", "C"]);
	git(&checkout, &["config", "user.email", "c@e"]);
	std::fs::write(checkout.join("a.txt"), b"changed\n").unwrap();
	git(&checkout, &["commit", "-aqm", "client change"]);
	let head = git(&checkout, &["rev-parse", "HEAD"]);
	let push = git_try(&checkout, &["push", "origin", "HEAD:refs/heads/main"]);
	assert!(
		push.status.success(),
		"git push failed: {}",
		String::from_utf8_lossy(&push.stderr)
	);

	let landed = open::<Sha1>(&git_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap();
	assert_eq!(landed, Some(ObjectId::<Sha1>::from_hex(&head).unwrap()));
}

// --- tags ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_clones_gitana_tags() {
	let work = unique_tmp("interop-tags");
	let (head, tag_id) = build_server::<Sha1>(&work.join("srv.git")).await;
	let url = serve_gitana(work.join("srv.git"), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, 2), 2);

	// Both tags land; the annotated tag is a tag object peeling to the commit, the lightweight tag
	// points straight at it.
	let tags = git(&checkout, &["tag"]);
	assert!(tags.contains("v1") && tags.contains("lw"), "tags: {tags}");
	assert_eq!(git(&checkout, &["rev-parse", "v1"]), tag_id.to_hex());
	assert_eq!(git(&checkout, &["rev-parse", "v1^{}"]), head.to_hex());
	assert_eq!(git(&checkout, &["rev-parse", "lw"]), head.to_hex());
}

// --- SHA-256 ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_clones_a_sha256_gitana_repo() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("interop-sha256");
	let (head, _) = build_server::<Sha256>(&work.join("srv.git")).await;
	let url = serve_gitana(work.join("srv.git"), ServerHash::Sha256).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, 2), 2);
	assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head.to_hex());
	assert_eq!(std::fs::read(checkout.join("a.txt")).unwrap(), b"hello\n");
}
