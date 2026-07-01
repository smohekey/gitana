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
use gitana_git_http::{ProtocolVersion, Service, advertise, receive_pack, upload_pack_v0};
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
	let body = advertise(&open(&git_dir), service, ProtocolVersion::V0, None)
		.await
		.expect("advertise");
	Bytes::from(body)
}

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
	Bytes::from(
		receive_pack(&open(&git_dir), &body, true)
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
