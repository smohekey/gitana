//! End-to-end Smart-HTTP round-trips for the `gta` client (`clone` / `fetch` / `push` / `pull`).
//!
//! There is no external git server here: a tiny in-process axum server serves gitana's OWN git-http
//! handlers (`advertise` / `upload_pack_v0` / `receive_pack`) over `http://127.0.0.1:<port>` against a
//! temp Sha256 server repo, and the real `gta` binary (subprocess) transacts against it. This is the
//! safety net for moving the remote composites into the engine — the client behaviour is exercised
//! end to end, over the wire.

use std::path::{Path, PathBuf};
use std::process::Output;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	NoReplayCheck, ProtocolVersion, ReceiveOptions, Service, TrustContext, advertise, receive_pack,
	upload_pack_v0,
};
use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use tempfile::TempDir;
use tokio::net::TcpListener;

/// A fixed identity for server-side commits (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

// --- the loopback Smart-HTTP server -------------------------------------------------------------

/// Open the server repo fresh from its git dir. Handlers re-open per request so pushes persist on
/// disk between requests; the tests never touch it concurrently.
fn open(git_dir: &Path) -> Repository<LocalFileStore, Sha256> {
	Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(git_dir),
	)))
}

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

/// `GET /info/refs?service=…` — the v0 ref advertisement.
async fn info_refs(State(git_dir): State<PathBuf>, RawQuery(query): RawQuery) -> Bytes {
	let raw = query.unwrap_or_default();
	let service = Service::parse(raw.strip_prefix("service=").unwrap_or(&raw)).expect("service");
	// Offer push-cert on receive-pack (advertise a nonce) so `gta push --signed` has something to sign.
	// Trust is not enforced here (`TrustContext::none()`), so the nonce is only echoed, not verified.
	let nonce = matches!(service, Service::ReceivePack).then_some(PUSH_CERT_NONCE);
	let body = advertise(&open(&git_dir), service, ProtocolVersion::V0, nonce)
		.await
		.expect("advertise");
	Bytes::from(body)
}

/// The push-cert nonce this test server advertises for receive-pack.
const PUSH_CERT_NONCE: &str = "1700000000-testnonce";

/// `POST /git-upload-pack` — v0 fetch/clone negotiation, responds with the packfile.
async fn upload_pack(State(git_dir): State<PathBuf>, body: Bytes) -> Bytes {
	Bytes::from(
		upload_pack_v0(&open(&git_dir), &body)
			.await
			.expect("upload-pack"),
	)
}

/// `POST /git-receive-pack` — push; force is on so the harness can also exercise non-ff / delete.
async fn git_receive_pack(State(git_dir): State<PathBuf>, body: Bytes) -> Bytes {
	// This harness exercises client push behavior, not trust; force on, no trust config.
	Bytes::from(
		receive_pack(
			&open(&git_dir),
			&body,
			ReceiveOptions {
				force: true,
				trust: &TrustContext::none(),
				now: 0,
				nonce_ledger: &NoReplayCheck,
			},
		)
		.await
		.expect("receive-pack")
		.report,
	)
}

/// Start the server over `git_dir` on an ephemeral port and return its base URL. The listener is
/// bound (and therefore accepting connections) before this returns, so there is no startup race.
async fn serve(git_dir: PathBuf) -> String {
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
		.route("/git-receive-pack", post(git_receive_pack))
		.with_state(git_dir);
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

// --- server-repo helpers ------------------------------------------------------------------------

/// Initialise a server repo at `git_dir` with one commit of `file`, returning its id.
async fn init_server(git_dir: &Path, file: &str, content: &[u8]) -> ObjectId<Sha256> {
	std::fs::create_dir_all(git_dir).unwrap();
	let repo = open(git_dir);
	repo.init().await.unwrap();
	commit_file(&repo, file, content).await
}

/// Record a commit of `file` = `content` on the server repo's `HEAD` branch.
async fn commit_file(
	repo: &Repository<LocalFileStore, Sha256>,
	file: &str,
	content: &[u8],
) -> ObjectId<Sha256> {
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

// --- gta client helpers -------------------------------------------------------------------------

/// Run the `gta` binary with `args` (subprocess), off the runtime so the server task keeps serving.
async fn gta(args: &[&str]) -> Output {
	let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
	tokio::task::spawn_blocking(move || {
		assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(&args)
			.output()
			.expect("run gta")
	})
	.await
	.unwrap()
}

fn ok(out: &Output, what: &str) {
	assert!(
		out.status.success(),
		"gta {what} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// The trimmed stdout of a successful `gta` invocation.
fn stdout(out: &Output, what: &str) -> String {
	ok(out, what);
	String::from_utf8(out.stdout.clone())
		.unwrap()
		.trim()
		.to_owned()
}

// --- the round-trips ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_checks_out_the_served_repo() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "hello.txt", b"world\n").await;
	let url = serve(git_dir).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	ok(
		&gta(&["clone", &url, target.to_str().unwrap()]).await,
		"clone",
	);

	assert_eq!(std::fs::read(target.join("hello.txt")).unwrap(), b"world\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_updates_the_remote_tracking_ref() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// The server gains a commit; a fetch should advance `refs/remotes/origin/main` to it.
	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");

	let tracking = stdout(
		&gta(&["-C", t, "rev-parse", "refs/remotes/origin/main"]).await,
		"rev-parse tracking",
	);
	assert_eq!(tracking, tip.to_hex());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_moves_the_server_ref() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(
		&gta(&["-C", t, "config", "user.name", "T"]).await,
		"config name",
	);
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"config email",
	);

	// A local commit, then push it.
	std::fs::write(target.join("f.txt"), b"2\n").unwrap();
	ok(&gta(&["-C", t, "add", "f.txt"]).await, "add");
	ok(
		&gta(&["-C", t, "commit", "-m", "local change"]).await,
		"commit",
	);
	let local_tip = stdout(
		&gta(&["-C", t, "rev-parse", "HEAD"]).await,
		"rev-parse HEAD",
	);

	ok(&gta(&["-C", t, "push"]).await, "push");

	// The server's branch now points at the pushed commit.
	let server_tip = open(&git_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap()
		.unwrap();
	assert_eq!(server_tip.to_hex(), local_tip);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_tags_sends_all_local_tags() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(&gta(&["-C", t, "config", "user.name", "T"]).await, "name");
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"email",
	);

	// A lightweight and an annotated tag on the current tip.
	ok(&gta(&["-C", t, "tag", "lw"]).await, "tag lw");
	ok(
		&gta(&["-C", t, "tag", "-a", "anno", "-m", "release"]).await,
		"tag anno",
	);
	let anno = stdout(
		&gta(&["-C", t, "rev-parse", "anno"]).await,
		"rev-parse anno",
	);

	// `--tags` pushes every local tag, lightweight and annotated alike.
	ok(&gta(&["-C", t, "push", "--tags"]).await, "push --tags");
	let server = open(&git_dir);
	let head = server
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap()
		.unwrap();
	assert_eq!(
		server.refs().resolve("refs/tags/lw").await.unwrap(),
		Some(head),
		"lightweight tag pushed"
	);
	assert_eq!(
		server
			.refs()
			.resolve("refs/tags/anno")
			.await
			.unwrap()
			.map(|o| o.to_hex()),
		Some(anno),
		"annotated tag object pushed"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_follow_tags_sends_only_reachable_annotated_tags() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(&gta(&["-C", t, "config", "user.name", "T"]).await, "name");
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"email",
	);

	// Advance the branch locally, then tag the new tip both ways.
	std::fs::write(target.join("f.txt"), b"2\n").unwrap();
	ok(&gta(&["-C", t, "add", "f.txt"]).await, "add");
	ok(&gta(&["-C", t, "commit", "-m", "local"]).await, "commit");
	let head = stdout(
		&gta(&["-C", t, "rev-parse", "HEAD"]).await,
		"rev-parse HEAD",
	);
	ok(&gta(&["-C", t, "tag", "lw"]).await, "tag lw");
	ok(
		&gta(&["-C", t, "tag", "-a", "anno", "-m", "release"]).await,
		"tag anno",
	);
	let anno = stdout(
		&gta(&["-C", t, "rev-parse", "anno"]).await,
		"rev-parse anno",
	);

	// `--follow-tags` pushes the branch plus the reachable *annotated* tag — not the lightweight one.
	ok(
		&gta(&["-C", t, "push", "--follow-tags"]).await,
		"push --follow-tags",
	);
	let server = open(&git_dir);
	assert_eq!(
		server
			.refs()
			.resolve("refs/heads/main")
			.await
			.unwrap()
			.map(|o| o.to_hex()),
		Some(head),
		"the branch advanced"
	);
	assert_eq!(
		server
			.refs()
			.resolve("refs/tags/anno")
			.await
			.unwrap()
			.map(|o| o.to_hex()),
		Some(anno),
		"the annotated tag was followed"
	);
	assert_eq!(
		server.refs().resolve("refs/tags/lw").await.unwrap(),
		None,
		"a lightweight tag is not followed"
	);
}

/// `gta push --signed` drives the real `ssh-keygen` signer end to end: it attaches a push certificate
/// (the server advertises a nonce), the signed path is taken (`(signed)` reported), and the ref moves.
/// The cryptographic correctness of the certificate — that its signature verifies under the signing
/// key in git's `git` namespace — is asserted at the porcelain level; here the value is the CLI wiring
/// (`--signed`/`--signing-key`, config resolution, the `ssh-keygen` subprocess) over a live server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_signed_moves_the_server_ref() {
	if !have_ssh_keygen() {
		eprintln!("skipping: ssh-keygen not available");
		return;
	}
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(&gta(&["-C", t, "config", "user.name", "T"]).await, "name");
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"email",
	);

	// An ephemeral ed25519 key; `gta push --signed` signs with `ssh-keygen -Y sign` under this key.
	let key = target.join("key");
	ssh_keygen(&[
		"-q",
		"-t",
		"ed25519",
		"-N",
		"",
		"-C",
		"t@e",
		"-f",
		key.to_str().unwrap(),
	]);
	ok(
		&gta(&["-C", t, "config", "gpg.format", "ssh"]).await,
		"gpg.format",
	);
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"user.signingkey",
			key.with_extension("pub").to_str().unwrap(),
		])
		.await,
		"signingkey",
	);

	std::fs::write(target.join("f.txt"), b"2\n").unwrap();
	ok(&gta(&["-C", t, "add", "f.txt"]).await, "add");
	ok(
		&gta(&["-C", t, "commit", "-m", "local change"]).await,
		"commit",
	);
	let local_tip = stdout(&gta(&["-C", t, "rev-parse", "HEAD"]).await, "rev-parse");

	// The signed push takes the certificate path and reports it.
	let out = stdout(&gta(&["-C", t, "push", "--signed"]).await, "push --signed");
	assert!(
		out.contains("(signed)"),
		"push did not report a signed push: {out}"
	);

	// The server's branch advanced to the pushed commit.
	let server_tip = open(&git_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap()
		.unwrap();
	assert_eq!(server_tip.to_hex(), local_tip);
}

/// Whether `ssh-keygen` is on `PATH` (SSH signing shells out to it); the signed-push test skips if not.
fn have_ssh_keygen() -> bool {
	std::process::Command::new("ssh-keygen")
		.arg("-?")
		.output()
		.is_ok()
}

/// Run `ssh-keygen` with `args`, asserting success (used to mint an ephemeral signing key).
fn ssh_keygen(args: &[&str]) {
	let out = std::process::Command::new("ssh-keygen")
		.args(args)
		.output()
		.expect("run ssh-keygen");
	assert!(
		out.status.success(),
		"ssh-keygen {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_fast_forwards_to_the_server() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// The server advances; pull should fast-forward the local branch and work tree.
	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "pull"]).await, "pull");

	let head = stdout(
		&gta(&["-C", t, "rev-parse", "HEAD"]).await,
		"rev-parse HEAD",
	);
	assert_eq!(head, tip.to_hex());
	assert_eq!(std::fs::read(target.join("f.txt")).unwrap(), b"2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_fast_forwards_under_a_mirror_refspec() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// A mirror refspec that maps the remote branch straight onto the local (checked-out) branch. A
	// plain fetch refuses this, but pull uses update-head-ok and reconciles the work tree via merge.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/*:refs/heads/*",
		])
		.await,
		"config mirror refspec",
	);

	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "pull"]).await, "pull mirror");

	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "HEAD"]).await,
			"rev-parse HEAD",
		),
		tip.to_hex()
	);
	assert_eq!(std::fs::read(target.join("f.txt")).unwrap(), b"2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_merges_the_refspec_mapped_source_not_the_same_name() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let base = init_server(&git_dir, "f.txt", b"1\n").await;
	// The server has an advanced `trunk` and a *stale* `main` (rewound to the base).
	let trunk_tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	{
		let repo = open(&git_dir);
		repo
			.refs()
			.update_ref("refs/heads/trunk", trunk_tip, None)
			.await
			.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/main", base, Some(trunk_tip))
			.await
			.unwrap();
	}
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// A rename refspec: the remote `trunk` is fetched onto the local checked-out `main`.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/trunk:refs/heads/main",
		])
		.await,
		"config rename refspec",
	);

	ok(&gta(&["-C", t, "pull"]).await, "pull rename");

	// Pull follows the mapped source (`trunk`), not the stale same-named `main`.
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "HEAD"]).await,
			"rev-parse HEAD",
		),
		trunk_tip.to_hex()
	);
	assert_eq!(std::fs::read(target.join("f.txt")).unwrap(), b"2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_declines_when_a_refspec_excludes_the_branch() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// A negative refspec excludes the current branch's remote ref; git declines to merge it.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/*:refs/remotes/origin/*",
		])
		.await,
		"config tracking",
	);
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"--add",
			"remote.origin.fetch",
			"^refs/heads/main",
		])
		.await,
		"config exclude",
	);

	let _tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	assert!(
		!gta(&["-C", t, "pull"]).await.status.success(),
		"pull must decline when the branch's remote ref is excluded"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_rejects_a_non_fast_forward_mirror_refspec() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(&gta(&["-C", t, "config", "user.name", "T"]).await, "name");
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"email",
	);
	// A *non-forced* mirror refspec into the checked-out branch.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"refs/heads/*:refs/heads/*",
		])
		.await,
		"config mirror",
	);

	// Diverge: a local commit, and an independent server commit (both children of the clone base).
	std::fs::write(target.join("f.txt"), b"local\n").unwrap();
	ok(&gta(&["-C", t, "add", "f.txt"]).await, "add");
	ok(&gta(&["-C", t, "commit", "-m", "local"]).await, "commit");
	let local_tip = stdout(
		&gta(&["-C", t, "rev-parse", "HEAD"]).await,
		"rev-parse local",
	);
	let _server = commit_file(&open(&git_dir), "f.txt", b"server\n").await;

	// The non-forced mirror refspec rejects the non-fast-forward onto the checked-out branch; pull
	// fails and the local branch is untouched (git: `! [rejected] ... (non-fast-forward)`).
	assert!(
		!gta(&["-C", t, "pull"]).await.status.success(),
		"pull must reject a non-fast-forward mirror refspec"
	);
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "HEAD"]).await,
			"rev-parse after",
		),
		local_tip
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_aborts_when_two_refspecs_target_the_checked_out_branch() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let root = init_server(&git_dir, "f.txt", b"1\n").await;
	open(&git_dir)
		.refs()
		.update_ref("refs/heads/dev", root, None)
		.await
		.unwrap();
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// Two different remote refs mapped onto the checked-out branch: git aborts, and pull must too
	// (the conflict is caught even though the destination is the checked-out branch).
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/main:refs/heads/main",
		])
		.await,
		"config refspec 1",
	);
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"--add",
			"remote.origin.fetch",
			"+refs/heads/dev:refs/heads/main",
		])
		.await,
		"config refspec 2",
	);

	assert!(
		!gta(&["-C", t, "pull"]).await.status.success(),
		"pull must abort on conflicting refspec destinations, even for the checked-out branch"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_refuses_a_forced_mirror_that_would_discard_local_commits() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	ok(&gta(&["-C", t, "config", "user.name", "T"]).await, "name");
	ok(
		&gta(&["-C", t, "config", "user.email", "t@e"]).await,
		"email",
	);
	// A *forced* mirror refspec: git would force-reset the checked-out branch (discarding the local
	// commit); gta declines that destructive update and refuses instead (safe-error policy).
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/*:refs/heads/*",
		])
		.await,
		"config forced mirror",
	);

	std::fs::write(target.join("f.txt"), b"local\n").unwrap();
	ok(&gta(&["-C", t, "add", "f.txt"]).await, "add");
	ok(&gta(&["-C", t, "commit", "-m", "local"]).await, "commit");
	let local_tip = stdout(
		&gta(&["-C", t, "rev-parse", "HEAD"]).await,
		"rev-parse local",
	);
	let _server = commit_file(&open(&git_dir), "f.txt", b"server\n").await;

	assert!(
		!gta(&["-C", t, "pull"]).await.status.success(),
		"pull must refuse a non-fast-forward forced mirror rather than discard local commits"
	);
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "HEAD"]).await,
			"rev-parse after",
		),
		local_tip,
		"the local commit must survive"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_honors_a_custom_tracking_namespace() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// Retarget the fetch refspec's destination to a custom namespace.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/*:refs/remotes/origin/mirror/*",
		])
		.await,
		"config fetch refspec",
	);

	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");

	// The tracking ref lands where the custom refspec maps it — not the default `origin/main`.
	let mirror = stdout(
		&gta(&["-C", t, "rev-parse", "refs/remotes/origin/mirror/main"]).await,
		"rev-parse custom tracking",
	);
	assert_eq!(mirror, tip.to_hex());
	assert!(
		!gta(&["-C", t, "rev-parse", "--verify", "refs/remotes/origin/main"])
			.await
			.status
			.success(),
		"the default tracking ref must not be written under a custom refspec"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_auto_follows_only_reachable_tags() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let root = init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// Build two tags on the server: `reach` on `main`'s tip (root), and `unreach` on a commit that is
	// NOT reachable from `main` — advance `main` to a child, tag it, then rewind `main` back to root.
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/reach", root, None)
		.await
		.unwrap();
	let child = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/unreach", child, None)
		.await
		.unwrap();
	open(&git_dir)
		.refs()
		.update_ref("refs/heads/main", root, Some(child))
		.await
		.unwrap();

	// A plain fetch auto-follows the tag reachable from `main`, but not the one on unfetched history.
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/reach"]).await,
			"rev-parse reach",
		),
		root.to_hex()
	);
	assert!(
		!gta(&["-C", t, "rev-parse", "--verify", "refs/tags/unreach"])
			.await
			.status
			.success(),
		"a tag on history this fetch did not pull must not be auto-followed"
	);

	// `--tags` mirrors every advertised tag regardless of reachability.
	ok(&gta(&["-C", t, "fetch", "--tags"]).await, "fetch --tags");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/unreach"]).await,
			"rev-parse unreach",
		),
		child.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_auto_follows_a_reachable_blob_tag() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// Tag the blob that is part of `main`'s tree (content-addressed, so re-writing it yields the same
	// id). Git auto-follows a tag pointing at ANY object reachable from the fetched branch — not only
	// commits — so a plain fetch must land this blob tag.
	let blob = open(&git_dir).write_blob(b"1\n").await.unwrap();
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/blobtag", blob, None)
		.await
		.unwrap();

	ok(&gta(&["-C", t, "fetch"]).await, "fetch");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/blobtag"]).await,
			"rev-parse blobtag",
		),
		blob.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_honors_tagopt_no_tags_config() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// `git clone --no-tags` records this; a plain fetch must then skip auto-follow.
	ok(
		&gta(&["-C", t, "config", "remote.origin.tagOpt", "--", "--no-tags"]).await,
		"config tagOpt",
	);

	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/v1", tip, None)
		.await
		.unwrap();

	// Plain fetch respects `tagOpt=--no-tags`: the reachable tag is not auto-followed.
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");
	assert!(
		!gta(&["-C", t, "rev-parse", "--verify", "refs/tags/v1"])
			.await
			.status
			.success(),
		"tagOpt=--no-tags must disable auto-follow"
	);

	// An explicit `--tags` on the command line overrides the config.
	ok(&gta(&["-C", t, "fetch", "--tags"]).await, "fetch --tags");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/v1"]).await,
			"rev-parse v1",
		),
		tip.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_no_tags_disables_auto_follow() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// The server advances `main` and tags the new tip — a tag that auto-follow would otherwise land.
	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/v1", tip, None)
		.await
		.unwrap();

	// `--no-tags` disables auto-follow: the tag is not written even though it is reachable.
	ok(
		&gta(&["-C", t, "fetch", "--no-tags"]).await,
		"fetch --no-tags",
	);
	assert!(
		!gta(&["-C", t, "rev-parse", "--verify", "refs/tags/v1"])
			.await
			.status
			.success(),
		"--no-tags must not auto-follow a reachable tag"
	);

	// A plain fetch then auto-follows it, confirming the tag was fetchable all along.
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/v1"]).await,
			"rev-parse v1",
		),
		tip.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_tags_refuses_to_clobber_a_moved_tag() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// The server tags its root commit; the client mirrors the tag.
	let root = open(&git_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap()
		.unwrap();
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/v1", root, None)
		.await
		.unwrap();
	ok(&gta(&["-C", t, "fetch", "--tags"]).await, "fetch --tags");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/v1"]).await,
			"tag first",
		),
		root.to_hex()
	);

	// The server repoints v1 to a descendant commit — a fast-forward for a branch, but tags are
	// immutable: a non-forced `--tags` fetch must reject it and leave the local tag at the root.
	let child = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	open(&git_dir)
		.refs()
		.update_ref("refs/tags/v1", child, Some(root))
		.await
		.unwrap();
	assert!(
		!gta(&["-C", t, "fetch", "--tags"]).await.status.success(),
		"repointing an existing tag without a force must fail the fetch"
	);
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/tags/v1"]).await,
			"tag after",
		),
		root.to_hex(),
		"the local tag must stay at its original target"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_rejects_a_non_fast_forward_without_force() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let root = init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// A non-forced refspec (no leading `+`): tracking updates only fast-forward.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"refs/heads/*:refs/remotes/origin/*",
		])
		.await,
		"config non-force refspec",
	);

	// Server advances to a child commit; the first fetch creates the tracking ref at it.
	let child = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "fetch"]).await, "fetch child");
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/remotes/origin/main"]).await,
			"rev-parse tracking",
		),
		child.to_hex()
	);

	// Rewind the server branch to the root — a non-fast-forward from the tracking ref's point of view.
	open(&git_dir)
		.refs()
		.update_ref("refs/heads/main", root, Some(child))
		.await
		.unwrap();
	// git treats the rejected update as a failed fetch (non-zero exit), even though nothing is written.
	assert!(
		!gta(&["-C", t, "fetch"]).await.status.success(),
		"a non-fast-forward under a non-forced refspec must fail the fetch"
	);

	// The non-forced refspec declines the rewind: the tracking ref still points at the child.
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/remotes/origin/main"]).await,
			"rev-parse tracking after rewind",
		),
		child.to_hex()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_applies_every_matching_refspec() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// Keep the default wildcard refspec and add a second, exact one for the same branch: git writes
	// both destinations.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"--add",
			"remote.origin.fetch",
			"+refs/heads/main:refs/remotes/origin/also-main",
		])
		.await,
		"config extra refspec",
	);

	let tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	ok(&gta(&["-C", t, "fetch"]).await, "fetch");

	for tracking in ["refs/remotes/origin/main", "refs/remotes/origin/also-main"] {
		assert_eq!(
			stdout(
				&gta(&["-C", t, "rev-parse", tracking]).await,
				"rev-parse tracking",
			),
			tip.to_hex(),
			"{tracking} should be written"
		);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_aborts_when_two_refspecs_target_one_ref() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let root = init_server(&git_dir, "f.txt", b"1\n").await;
	// A second server branch so both refspecs below have a source to match.
	open(&git_dir)
		.refs()
		.update_ref("refs/heads/dev", root, None)
		.await
		.unwrap();
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");

	// Two refspecs mapping different branches to the same tracking ref: git aborts the whole fetch.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/main:refs/remotes/origin/same",
		])
		.await,
		"config refspec 1",
	);
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"--add",
			"remote.origin.fetch",
			"+refs/heads/dev:refs/remotes/origin/same",
		])
		.await,
		"config refspec 2",
	);

	assert!(
		!gta(&["-C", t, "fetch"]).await.status.success(),
		"conflicting refspec destinations must fail the fetch"
	);
	// The conflict is caught before any write: the destination ref was not created.
	assert!(
		!gta(&["-C", t, "rev-parse", "--verify", "refs/remotes/origin/same"])
			.await
			.status
			.success(),
		"no tracking ref should be written when the config conflicts"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_fails_on_an_absent_exact_source() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	// An exact source the remote does not advertise (a typo or deleted branch): git errors.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/nonexistent:refs/remotes/origin/nope",
		])
		.await,
		"config bad source",
	);

	assert!(
		!gta(&["-C", t, "fetch"]).await.status.success(),
		"fetch must fail when an exact source ref is not advertised"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_refuses_to_write_the_checked_out_branch() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	let dst = TempDir::new().unwrap();
	let target = dst.path().join("clone");
	let t = target.to_str().unwrap();
	ok(&gta(&["clone", &url, t]).await, "clone");
	let before = stdout(
		&gta(&["-C", t, "rev-parse", "refs/heads/main"]).await,
		"rev-parse before",
	);

	// A refspec that maps the remote branch straight onto the local checked-out branch: git refuses,
	// because a plain fetch does not update the work tree.
	ok(
		&gta(&[
			"-C",
			t,
			"config",
			"remote.origin.fetch",
			"+refs/heads/*:refs/heads/*",
		])
		.await,
		"config self-refspec",
	);

	let _tip = commit_file(&open(&git_dir), "f.txt", b"2\n").await;
	assert!(
		!gta(&["-C", t, "fetch"]).await.status.success(),
		"fetching into the checked-out branch must fail"
	);
	// The local branch tip is untouched.
	assert_eq!(
		stdout(
			&gta(&["-C", t, "rev-parse", "refs/heads/main"]).await,
			"rev-parse after",
		),
		before
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_repo_fetches_into_its_branch_namespace() {
	let srv = TempDir::new().unwrap();
	let git_dir = srv.path().join("srv.git");
	let root = init_server(&git_dir, "f.txt", b"1\n").await;
	let url = serve(git_dir.clone()).await;

	// A bare client (no work tree) with a mirror refspec into `refs/heads/*`. Configure it through the
	// repository API directly — `core.bare` must already be set for the CLI to discover the bare dir.
	let client = srv.path().join("client.git");
	std::fs::create_dir_all(&client).unwrap();
	let crepo = open(&client);
	crepo.init().await.unwrap();
	// `discover` recognises a bare dir by `HEAD` + `objects/` + `refs/`; ensure the dirs exist.
	std::fs::create_dir_all(client.join("objects/pack")).unwrap();
	std::fs::create_dir_all(client.join("refs/heads")).unwrap();
	let mut cfg = crepo.read_config().await.unwrap();
	cfg.set("core", None, "bare", "true").unwrap();
	cfg.set("remote", Some("origin"), "url", &url).unwrap();
	cfg
		.set(
			"remote",
			Some("origin"),
			"fetch",
			"+refs/heads/*:refs/heads/*",
		)
		.unwrap();
	crepo.write_config(&cfg).await.unwrap();
	let c = client.to_str().unwrap();

	// git permits fetching straight into a bare repo's branch namespace; the branch is created.
	ok(&gta(&["-C", c, "fetch"]).await, "bare fetch");
	assert_eq!(
		stdout(
			&gta(&["-C", c, "rev-parse", "refs/heads/main"]).await,
			"rev-parse bare main",
		),
		root.to_hex()
	);
}
