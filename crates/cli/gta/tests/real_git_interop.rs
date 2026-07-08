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
use support::{
	ServerHash, git, git_supports_sha256, git_try, gta, gta_ok, open, serve_gitana, unique_tmp,
};

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

/// A fetch from gitana with a divergent local history exercises **multi-round** negotiation: the
/// client's first `have` batches are its unrelated local commits, so gitana keeps acknowledging (never
/// prematurely `ready`) until the shared base is offered, then sends the pack. Proves the multi-round
/// ACK negotiation interoperates with a real git client, and the delivered result is complete.
async fn multi_round_fetch_case(version: u8) {
	let work = unique_tmp("interop-multiround");
	let git_dir = work.join("srv.git");
	build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, version), version);
	git(&checkout, &["config", "user.name", "C"]);
	git(&checkout, &["config", "user.email", "c@e"]);

	// A long divergent local branch: its (newest) commits fill the first have-batches with objects the
	// server does not share, forcing more than one negotiation round before the common base is reached.
	git(&checkout, &["checkout", "-qb", "work"]);
	for i in 0..50 {
		git(
			&checkout,
			&["commit", "-q", "--allow-empty", "-m", &format!("d{i}")],
		);
	}
	git(&checkout, &["checkout", "-q", "main"]);

	// The server advances `main`; the fetch must negotiate through the divergence and land the new tip.
	let advanced = commit_on(&open::<Sha1>(&git_dir), "b.txt", b"more\n").await;
	let out = git_try(
		&checkout,
		&[
			"-c",
			&format!("protocol.version={version}"),
			"fetch",
			"origin",
			"main",
		],
	);
	assert!(
		out.status.success(),
		"multi-round fetch (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		git(&checkout, &["rev-parse", "FETCH_HEAD"]),
		advanced.to_hex(),
		"the fetch landed the advanced tip after negotiating past the divergence"
	);
	let fsck = git_try(&checkout, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after multi-round fetch (v{version}) failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_multi_round_fetches_gitana_over_v2() {
	multi_round_fetch_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_multi_round_fetches_gitana_over_v0() {
	multi_round_fetch_case(0).await;
}

/// A non-shallow single-branch clone honours `include-tag`: the annotated tag reachable from the
/// fetched branch is delivered even though the client only `want`s the branch tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_single_branch_clone_gets_reachable_tags_from_gitana() {
	let work = unique_tmp("interop-single-branch");
	let git_dir = work.join("srv.git");
	let (_head, tag_id) = build_server::<Sha1>(&git_dir).await;
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			"protocol.version=2",
			"clone",
			"--single-branch",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"single-branch clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// The annotated tag on the fetched branch is delivered via include-tag.
	assert_eq!(git(&checkout, &["rev-parse", "v1"]), tag_id.to_hex());
}

// --- shallow (--depth / --shallow-exclude) ------------------------------------------------------

/// A server repo with a linear history of `n` commits on `main` (`a.txt` = `v0`, `v1`, …); returns the
/// commit ids oldest→newest.
async fn build_linear_server<H: HashAlgorithm>(git_dir: &Path, n: usize) -> Vec<ObjectId<H>> {
	std::fs::create_dir_all(git_dir).unwrap();
	let repo = open::<H>(git_dir);
	repo.init().await.unwrap();
	let mut ids = Vec::new();
	for i in 0..n {
		ids.push(commit_on(&repo, "a.txt", format!("v{i}\n").as_bytes()).await);
	}
	ids
}

/// The trimmed contents of a checkout's `.git/shallow`.
fn shallow_file(checkout: &Path) -> String {
	std::fs::read_to_string(checkout.join(".git/shallow"))
		.unwrap_or_default()
		.trim()
		.to_owned()
}

/// Stock `git clone --depth N` from gitana truncates history exactly: the boundary is the depth-N
/// frontier, the objects past it are absent, and `git fsck` accepts the shallow repo.
async fn shallow_depth_case(version: u8) {
	let work = unique_tmp("interop-shallow");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;
	let (grandparent, parent, tip) = (ids[0].to_hex(), ids[1].to_hex(), ids[2].to_hex());

	// --depth 1: only the tip; the boundary is the tip and the parent is absent.
	let c1 = work.join("d1");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			"--depth",
			"1",
			&url,
			c1.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone --depth 1 (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(git(&c1, &["rev-parse", "HEAD"]), tip);
	assert_eq!(git(&c1, &["rev-list", "--count", "HEAD"]), "1");
	assert_eq!(shallow_file(&c1), tip);
	assert!(!git_try(&c1, &["cat-file", "-t", &parent]).status.success());
	let fsck = git_try(&c1, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after --depth 1 (v{version}) failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);

	// --depth 2: tip + parent; the parent is the boundary and the root is absent.
	let c2 = work.join("d2");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			"--depth",
			"2",
			&url,
			c2.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone --depth 2 (v{version}) failed"
	);
	assert_eq!(git(&c2, &["rev-list", "--count", "HEAD"]), "2");
	assert_eq!(shallow_file(&c2), parent);
	assert!(git_try(&c2, &["cat-file", "-t", &parent]).status.success());
	assert!(
		!git_try(&c2, &["cat-file", "-t", &grandparent])
			.status
			.success()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_clones_gitana_over_v2() {
	shallow_depth_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_clones_gitana_over_v0() {
	shallow_depth_case(0).await;
}

/// `git clone --shallow-exclude=<ref>` from gitana omits history reachable from that ref: the server
/// honours `deepen-not`, cutting at the commit whose parent is the excluded tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_excludes_a_ref_from_gitana() {
	let work = unique_tmp("interop-shallow-exclude");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let repo = open::<Sha1>(&git_dir);
	repo
		.refs()
		.update_ref("refs/tags/mark", ids[1], None) // mark = c1
		.await
		.unwrap();
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;
	let (parent, tip) = (ids[1].to_hex(), ids[2].to_hex());

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			"protocol.version=2",
			"clone",
			"--shallow-exclude=mark",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone --shallow-exclude failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// Only the tip is present (its parent c1 is reachable from `mark`, so excluded); the tip is shallow.
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "1");
	assert_eq!(shallow_file(&checkout), tip);
	assert!(
		!git_try(&checkout, &["cat-file", "-t", &parent])
			.status
			.success()
	);
}

/// A shallow clone from gitana honours `include-tag`: an annotated tag whose target is within the depth
/// is delivered and its ref created, while a tag on the pruned history is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_clone_keeps_a_reachable_tag_from_gitana() {
	let work = unique_tmp("interop-shallow-tag");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 2).await; // c0 <- c1 (tip)
	let repo = open::<Sha1>(&git_dir);
	let annotate = |name: &str, target: ObjectId<Sha1>| Tag::<Sha1> {
		object: target,
		kind: ObjectKind::Commit,
		name: name.to_owned(),
		tagger: Some(WHO.to_owned()),
		signature: None,
		message: format!("{name}\n"),
	};
	let near = repo
		.objects()
		.write_object(ObjectKind::Tag, &encode_tag(&annotate("v1", ids[1])))
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/tags/v1", near, None)
		.await
		.unwrap();
	let far = repo
		.objects()
		.write_object(ObjectKind::Tag, &encode_tag(&annotate("old", ids[0])))
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/tags/old", far, None)
		.await
		.unwrap();

	let url = serve_gitana(git_dir, ServerHash::Sha1).await;
	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			"protocol.version=2",
			"clone",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// The annotated tag within depth is delivered (include-tag) and created; the far one is absent.
	assert_eq!(git(&checkout, &["rev-parse", "v1"]), near.to_hex());
	assert!(!git_try(&checkout, &["rev-parse", "old"]).status.success());
}

/// A normal fetch from a shallow clone stays shallow: the server sends the new commit but does not
/// unshallow or re-plan the boundary (it emits no `shallow-info`).
async fn shallow_then_fetch_case(version: u8) {
	let work = unique_tmp("interop-shallow-fetch");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 2).await; // c0 <- c1 (tip)
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	// The server advances; a plain fetch follows it but the repo stays shallow at the original tip.
	let advanced = commit_on(&open::<Sha1>(&git_dir), "a.txt", b"v2\n").await;
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
		"fetch after shallow clone (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		git(&checkout, &["rev-parse", "origin/main"]),
		advanced.to_hex()
	);
	// Still shallow at c1 (its parent c0 remains absent).
	assert_eq!(shallow_file(&checkout), ids[1].to_hex());
	assert!(
		!git_try(&checkout, &["cat-file", "-t", &ids[0].to_hex()])
			.status
			.success()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_after_shallow_clone_over_v2() {
	shallow_then_fetch_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_after_shallow_clone_over_v0() {
	shallow_then_fetch_case(0).await;
}

/// `git fetch --unshallow` from gitana fills in the truncated history: the server sends the
/// newly-exposed ancestors and unshallows the boundary, leaving a complete repo.
async fn unshallow_case(version: u8) {
	let work = unique_tmp("interop-unshallow");
	let git_dir = work.join("srv.git");
	build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "shallow clone (v{version}) failed");
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "1");

	let out = git_try(
		&checkout,
		&[
			"-c",
			&format!("protocol.version={version}"),
			"fetch",
			"--unshallow",
			"origin",
		],
	);
	assert!(
		out.status.success(),
		"unshallow (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// The full history is present and the repo is no longer shallow.
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "3");
	assert!(!checkout.join(".git/shallow").exists());
	let fsck = git_try(&checkout, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after unshallow (v{version}) failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_unshallows_gitana_over_v2() {
	unshallow_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_unshallows_gitana_over_v0() {
	unshallow_case(0).await;
}

/// `git fetch --unshallow` against gitana completes *every* client boundary, including one for a branch
/// the (narrowed) fetch refspec no longer selects: git sends `shallow` lines for its whole boundary and
/// the gitana server unshallows all of them from those lines — not just the ones its `want`s reach.
async fn narrowed_unshallow_case(version: u8) {
	let work = unique_tmp("interop-narrowed-unshallow");
	let git_dir = work.join("srv.git");
	build_linear_server::<Sha1>(&git_dir, 2).await; // main: c0 <- c1 (tip)

	// A disjoint `other` branch: x0 <- x1.
	let repo = open::<Sha1>(&git_dir);
	let entry = |id| TreeBuildEntry {
		path: "b.txt".to_owned(),
		mode: FileMode::Regular,
		id,
	};
	let b0 = repo.write_blob(b"o0\n").await.unwrap();
	let t0 = repo.write_tree(&[entry(b0)]).await.unwrap();
	let x0 = repo
		.create_commit(t0, vec![], WHO, WHO, "o0\n")
		.await
		.unwrap();
	let b1 = repo.write_blob(b"o1\n").await.unwrap();
	let t1 = repo.write_tree(&[entry(b1)]).await.unwrap();
	let x1 = repo
		.create_commit(t1, vec![x0], WHO, WHO, "o1\n")
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/heads/other", x1, None)
		.await
		.unwrap();

	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	// A multi-branch depth-1 clone: both `main` and `other` are shallow.
	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&format!("protocol.version={version}"),
			"clone",
			"--no-single-branch",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"multi-branch shallow clone (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		shallow_file(&checkout).lines().count(),
		2,
		"both branches are shallow"
	);

	// Narrow the refspec to `main`, then unshallow. `other` is no longer selected by a want.
	git(
		&checkout,
		&[
			"config",
			"remote.origin.fetch",
			"+refs/heads/main:refs/remotes/origin/main",
		],
	);
	let out = git_try(
		&checkout,
		&[
			"-c",
			&format!("protocol.version={version}"),
			"fetch",
			"--unshallow",
			"origin",
		],
	);
	assert!(
		out.status.success(),
		"narrowed unshallow (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// The repo is fully complete — no `.git/shallow` — and the unselected branch's history is present,
	// so git fsck accepts it.
	assert!(
		!checkout.join(".git/shallow").exists(),
		"the unselected branch's boundary must also be completed"
	);
	assert_eq!(
		git(&checkout, &["rev-list", "--count", &x1.to_hex()]),
		"2",
		"the unselected branch's full history is present"
	);
	let fsck = git_try(&checkout, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after narrowed unshallow (v{version}) failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_narrowed_unshallow_completes_all_boundaries_over_v2() {
	narrowed_unshallow_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_narrowed_unshallow_completes_all_boundaries_over_v0() {
	narrowed_unshallow_case(0).await;
}

/// A shallow client fetching an *older* commit it does not have: the server must not subtract that
/// commit through the client's shallow boundary. Regression for the have-walk bounding on non-deepen
/// fetches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_client_fetches_an_older_branch_from_gitana() {
	let work = unique_tmp("interop-shallow-older");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (main tip)
	let repo = open::<Sha1>(&git_dir);
	// A branch pointing at the root, which a depth-1 clone of `main` will not have.
	repo
		.refs()
		.update_ref("refs/heads/base", ids[0], None)
		.await
		.unwrap();
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			"protocol.version=2",
			"clone",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "shallow clone failed");

	// Fetch the old branch: the server must send c0 (the client lacks it), not omit it as "already had".
	let out = git_try(
		&checkout,
		&[
			"-c",
			"protocol.version=2",
			"fetch",
			"origin",
			"base:refs/remotes/origin/base",
		],
	);
	assert!(
		out.status.success(),
		"fetching an older branch from a shallow clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(
		git(
			&checkout,
			&["cat-file", "-p", &format!("{}:a.txt", ids[0].to_hex())]
		),
		"v0"
	);
}

/// `git clone --depth 1 --branch <annotated-tag>` from gitana honours the depth: the tag's commit is
/// the boundary rather than the whole ancestry (the want is peeled to its commit before planning).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_shallow_clones_an_annotated_tag_from_gitana() {
	let work = unique_tmp("interop-shallow-tag-depth");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let repo = open::<Sha1>(&git_dir);
	let tag = Tag::<Sha1> {
		object: ids[2],
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
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			"protocol.version=2",
			"clone",
			"--depth",
			"1",
			"--branch",
			"v1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"shallow clone of an annotated tag failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	// Depth honoured: only the tag's commit, shallow at it; the parent is absent.
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "1");
	assert_eq!(shallow_file(&checkout), ids[2].to_hex());
	assert!(
		!git_try(&checkout, &["cat-file", "-t", &ids[1].to_hex()])
			.status
			.success()
	);
}

/// `git fetch --deepen=N` (relative deepening) from gitana extends the history by N more commits below
/// the client's current boundary, one step at a time.
async fn deepen_case(version: u8) {
	let work = unique_tmp("interop-deepen");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 4).await; // c0 <- c1 <- c2 <- c3 (tip)
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let proto = format!("protocol.version={version}");
	let out = git_try(
		Path::new("."),
		&[
			"-c",
			&proto,
			"clone",
			"--depth",
			"1",
			&url,
			checkout.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "shallow clone (v{version}) failed");
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "1");

	// Deepen by one level: the boundary moves to c2.
	let out = git_try(
		&checkout,
		&["-c", &proto, "fetch", "--deepen", "1", "origin"],
	);
	assert!(
		out.status.success(),
		"deepen 1 (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "2");
	assert_eq!(shallow_file(&checkout), ids[2].to_hex());

	// Deepen by one more: the boundary moves to c1.
	let out = git_try(
		&checkout,
		&["-c", &proto, "fetch", "--deepen", "1", "origin"],
	);
	assert!(
		out.status.success(),
		"deepen 1 again (v{version}) failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "3");
	assert_eq!(shallow_file(&checkout), ids[1].to_hex());
	let fsck = git_try(&checkout, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after deepen (v{version}) failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_deepens_gitana_over_v2() {
	deepen_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_deepens_gitana_over_v0() {
	deepen_case(0).await;
}

/// End-to-end shallow over gitana's own transport: the gitana `gta` client shallow-clones a gitana
/// server (5c-1 client + 5c-2 server). The server now advertises `shallow`, so the client's capability
/// gate passes and `.git/shallow` is written to the same boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gta_shallow_clones_gitana() {
	let work = unique_tmp("interop-gta-shallow");
	let git_dir = work.join("srv.git");
	let ids = build_linear_server::<Sha1>(&git_dir, 3).await; // c0 <- c1 <- c2 (tip)
	let url = serve_gitana(git_dir, ServerHash::Sha1).await;

	let checkout = work.join("c");
	let c = checkout.to_str().unwrap();
	gta_ok(
		&gta(&["clone", "--depth", "1", &url, c]).await,
		"gta shallow clone",
	);
	// gta wrote the boundary and truncated history to the tip.
	assert_eq!(shallow_file(&checkout), ids[2].to_hex());
	assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), ids[2].to_hex());
	assert!(
		!git_try(&checkout, &["cat-file", "-t", &ids[1].to_hex()])
			.status
			.success()
	);
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

// --- thin packs ---------------------------------------------------------------------------------

/// A large, delta-friendly blob tagged by `marker`; a successor differing in one byte deltas well
/// against it, forcing a thin pack when one side already has the other version.
fn big(marker: u8) -> Vec<u8> {
	let mut data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
	data.push(marker);
	data
}

/// Stock `git` fetches an incremental change to a big file from gitana. git negotiates `thin-pack`
/// by default, so gitana serves a **thin** pack (the new blob as a REF delta against the old one the
/// client already has); git must complete it (`index-pack --fix-thin`). fsck proves the result is
/// intact and connected.
async fn thin_fetch_case(version: u8) {
	let work = unique_tmp("interop-thin-fetch");
	let git_dir = work.join("srv.git");
	std::fs::create_dir_all(&git_dir).unwrap();
	let repo = open::<Sha1>(&git_dir);
	repo.init().await.unwrap();
	commit_on(&repo, "big.txt", &big(b'A')).await;
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, version), version);

	// The server changes the big file by one byte; the client fetches it.
	let advanced = commit_on(&open::<Sha1>(&git_dir), "big.txt", &big(b'B')).await;
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
	// A completed thin pack must be a valid, connected object graph.
	let fsck = git_try(&checkout, &["fsck", "--full"]);
	assert!(
		fsck.status.success(),
		"git fsck after a thin fetch failed: {}",
		String::from_utf8_lossy(&fsck.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_a_thin_pack_from_gitana_over_v2() {
	thin_fetch_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_fetches_a_thin_pack_from_gitana_over_v0() {
	thin_fetch_case(0).await;
}

/// Stock `git` pushes an incremental change to a big file into gitana. git's send-pack sends a
/// **thin** pack by default (the new blob as a REF delta against the old one gitana has); gitana's
/// receive-pack must complete it against its store. The push succeeding at all requires connectivity
/// to hold; we further read the de-thinned blob back to confirm it was stored byte-for-byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_pushes_a_thin_pack_into_gitana() {
	let work = unique_tmp("interop-thin-push");
	let git_dir = work.join("srv.git");
	std::fs::create_dir_all(&git_dir).unwrap();
	let repo = open::<Sha1>(&git_dir);
	repo.init().await.unwrap();
	commit_on(&repo, "big.txt", &big(b'A')).await;
	let url = serve_gitana(git_dir.clone(), ServerHash::Sha1).await;

	let checkout = work.join("c");
	assert_cloned(&clone(&url, &checkout, 2), 2);

	// Change the big file by one byte through stock git and push it back.
	git(&checkout, &["config", "user.name", "C"]);
	git(&checkout, &["config", "user.email", "c@e"]);
	std::fs::write(checkout.join("big.txt"), big(b'B')).unwrap();
	git(&checkout, &["commit", "-aqm", "tweak big"]);
	let head = git(&checkout, &["rev-parse", "HEAD"]);
	let blob = git(&checkout, &["rev-parse", "HEAD:big.txt"]);
	let push = git_try(&checkout, &["push", "origin", "HEAD:refs/heads/main"]);
	assert!(
		push.status.success(),
		"git thin push failed: {}",
		String::from_utf8_lossy(&push.stderr)
	);

	// The ref moved, and the delta-completed blob is stored intact.
	let server = open::<Sha1>(&git_dir);
	assert_eq!(
		server.refs().resolve("refs/heads/main").await.unwrap(),
		Some(ObjectId::<Sha1>::from_hex(&head).unwrap())
	);
	let (kind, data) = server
		.objects()
		.read_object(&ObjectId::<Sha1>::from_hex(&blob).unwrap())
		.await
		.unwrap();
	assert_eq!(kind, ObjectKind::Blob);
	assert_eq!(data, big(b'B'));
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
