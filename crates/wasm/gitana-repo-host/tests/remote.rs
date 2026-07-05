//! End-to-end proof of the in-component remote porcelain over `wasi:http`: `fetch`, `clone`, `push`.
//!
//! A loopback axum server serves gitana's OWN Smart-HTTP handlers (`advertise` / `upload_pack_v0` /
//! `receive_pack`) over `http://127.0.0.1:<port>` against a temp server repo. The component —
//! instantiated with **no preopens**, its only network authority the host-granted
//! `wasi:http/outgoing-handler` — is granted the client's directory descriptors and asked to fetch,
//! clone, or push against that URL. The whole transport path runs in the reactor: every advertisement
//! GET and pack POST flows through the in-guest `WasiHttpTransport`, blocking inline on `wasi:io`
//! pollables under the sync-export `block_on`. We check refs advanced, objects landed on disk, and
//! (for clone) the working tree materialised — in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	ProtocolVersion, ReceiveOptions, Service, TrustContext, advertise, receive_pack, upload_pack_v0,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repo_host::exports::gitana::repo::porcelain::PushOutcome;
use gitana_repo_host::{engine, grant_dir, instantiate, store};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use tokio::net::TcpListener;

use self::support::{Session, build_component, native_repo};

/// A fixed identity for server-side commits (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

// --- the loopback Smart-HTTP server -------------------------------------------------------------

/// The server repo dir plus its object format. The handlers are non-generic (their futures must be
/// `Send` for axum) and dispatch the hash algorithm off `kind` at runtime.
#[derive(Clone)]
struct ServerState {
	git_dir: PathBuf,
	kind: HashKind,
}

/// Open a repo fresh from its git dir as hash `H` (handlers re-open per request).
fn open<H: HashAlgorithm>(git_dir: &Path) -> Repository<LocalFileStore, H> {
	native_repo::<H>(git_dir).expect("open repo")
}

/// `GET /info/refs?service=…` — the v0 ref advertisement.
async fn info_refs(State(st): State<ServerState>, RawQuery(query): RawQuery) -> Bytes {
	let raw = query.unwrap_or_default();
	let service = Service::parse(raw.strip_prefix("service=").unwrap_or(&raw)).expect("service");
	let body = match st.kind {
		HashKind::Sha1 => {
			advertise(
				&open::<Sha1>(&st.git_dir),
				service,
				ProtocolVersion::V0,
				None,
			)
			.await
		}
		HashKind::Sha256 => {
			advertise(
				&open::<Sha256>(&st.git_dir),
				service,
				ProtocolVersion::V0,
				None,
			)
			.await
		}
	}
	.expect("advertise");
	Bytes::from(body)
}

/// `POST /git-upload-pack` — v0 fetch/clone negotiation, responds with the packfile.
async fn upload_pack(State(st): State<ServerState>, body: Bytes) -> Bytes {
	let pack = match st.kind {
		HashKind::Sha1 => upload_pack_v0(&open::<Sha1>(&st.git_dir), &body).await,
		HashKind::Sha256 => upload_pack_v0(&open::<Sha256>(&st.git_dir), &body).await,
	}
	.expect("upload-pack");
	Bytes::from(pack)
}

/// `POST /git-receive-pack` — push: unpack, validate, move refs, and report status. `force` is on
/// so the server would accept a non-fast-forward rewrite — which is what makes the client-side
/// force check observable (a rejected non-ff push means the *client* declined, not the server).
async fn receive_pack_srv(State(st): State<ServerState>, body: Bytes) -> Bytes {
	// This harness exercises remote transport, not trust; push with force and no trust config.
	let no_trust = TrustContext::none();
	let options = || ReceiveOptions {
		force: true,
		trust: &no_trust,
		now: 0,
	};
	let report = match st.kind {
		HashKind::Sha1 => {
			receive_pack(&open::<Sha1>(&st.git_dir), &body, options())
				.await
				.expect("receive-pack")
				.report
		}
		HashKind::Sha256 => {
			receive_pack(&open::<Sha256>(&st.git_dir), &body, options())
				.await
				.expect("receive-pack")
				.report
		}
	};
	Bytes::from(report)
}

/// Start the server over `git_dir` on an ephemeral port; the listener is bound before this returns,
/// so there is no startup race with the fetch below.
async fn serve(git_dir: PathBuf, kind: HashKind) -> String {
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
		.route("/git-receive-pack", post(receive_pack_srv))
		.with_state(ServerState { git_dir, kind });
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

/// The object format `H` names, as a runtime [`HashKind`] for the server state.
fn kind_of<H: HashAlgorithm>() -> HashKind {
	if H::NAME == Sha256::NAME {
		HashKind::Sha256
	} else {
		HashKind::Sha1
	}
}

/// Record a commit of `file` = `content` on the server repo's `HEAD` branch.
async fn commit_file<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
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

// --- the round-trip -----------------------------------------------------------------------------

/// The component fetches a one-commit server repo and lands it in an empty client — tracking ref
/// advanced, objects on disk.
async fn fetch_advances_tracking_ref<H: HashAlgorithm>() -> Result<()> {
	// A server repo with one commit on `main`.
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	let tip = commit_file(&server, "hello.txt", b"world\n").await;
	let url = serve(server_git, kind_of::<H>()).await;

	// An empty client repo of the same object format.
	let cli = tempfile::tempdir()?;
	let client_git = cli.path().join("client.git");
	std::fs::create_dir_all(&client_git)?;
	open::<H>(&client_git).init().await?;

	// Fetch through the component, over wasi:http.
	let mut session = Session::open(&client_git).await?;
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut session.store, session.handle, &url)
		.await?
		.map_err(|error| anyhow!("fetch: {error:?}"))?;

	// The default refspec advanced `refs/remotes/origin/main` to the server tip.
	assert!(
		outcome
			.updated
			.iter()
			.any(|r| r.name == "refs/remotes/origin/main" && r.id == tip.to_hex()),
		"expected refs/remotes/origin/main at {}, got {:?}",
		tip.to_hex(),
		outcome.updated
	);
	assert!(
		outcome.rejected.is_empty(),
		"unexpected rejects: {:?}",
		outcome.rejected
	);

	// The pack actually landed: natively the client now resolves the tracking ref to the tip and can
	// read the fetched commit's tree (proving the objects transferred, not just the ref).
	let client = native_repo::<H>(&client_git)?;
	assert_eq!(
		client.refs().resolve("refs/remotes/origin/main").await?,
		Some(tip)
	);
	client.commit_tree(tip).await?;
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_sha256() {
	fetch_advances_tracking_ref::<Sha256>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_sha1() {
	fetch_advances_tracking_ref::<Sha1>().await.unwrap();
}

/// The component clones a one-commit server repo into an empty checkout — objects land in the git
/// dir, `HEAD` resolves to the server tip, the working tree is materialised, and the origin saved.
async fn clone_populates_checkout<H: HashAlgorithm>() -> Result<()> {
	// A server repo with one commit on `main`.
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	let tip = commit_file(&server, "hello.txt", b"world\n").await;
	let url = serve(server_git, kind_of::<H>()).await;

	// An empty client checkout: the working dir and its `.git`, both empty.
	let cli = tempfile::tempdir()?;
	let work = cli.path().join("checkout");
	let git = work.join(".git");
	std::fs::create_dir_all(&git)?;

	// Clone through the component, over wasi:http. `clone` is a static func that consumes the two
	// granted descriptors and returns unit (a clone populates directories, it does not open one).
	let engine = engine()?;
	let mut store = store(&engine);
	let repo = instantiate(&engine, &mut store, build_component()).await?;
	let git_desc = grant_dir(&mut store, &git)?;
	let work_desc = grant_dir(&mut store, &work)?;
	repo
		.gitana_repo_porcelain()
		.repository()
		.call_clone(&mut store, git_desc, work_desc, &url)
		.await?
		.map_err(|error| anyhow!("clone: {error:?}"))?;

	// Natively: the client resolves HEAD to the server tip and can read the fetched tree (objects
	// transferred, not just refs).
	let client = native_repo::<H>(&git)?;
	assert_eq!(client.refs().resolve_head().await?, Some(tip));
	client.commit_tree(tip).await?;

	// The working tree was materialised and the origin persisted through the descriptor file store.
	assert_eq!(std::fs::read(work.join("hello.txt"))?, b"world\n");
	let config = std::fs::read_to_string(git.join("config"))?;
	assert!(
		config.contains(&format!("url = {url}")),
		"config missing origin url:\n{config}"
	);
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_sha256() {
	clone_populates_checkout::<Sha256>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_sha1() {
	clone_populates_checkout::<Sha1>().await.unwrap();
}

/// The component pushes its `HEAD` branch to an empty server repo — the branch is created on the
/// server at the client tip, and the packed objects land there.
async fn push_creates_remote_branch<H: HashAlgorithm>() -> Result<()> {
	// An empty server repo (no refs yet): receive-pack will create `main`.
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	open::<H>(&server_git).init().await?;
	let url = serve(server_git.clone(), kind_of::<H>()).await;

	// A client repo with one commit on `main`.
	let cli = tempfile::tempdir()?;
	let client_git = cli.path().join("client.git");
	std::fs::create_dir_all(&client_git)?;
	let client = open::<H>(&client_git);
	client.init().await?;
	let tip = commit_file(&client, "hello.txt", b"world\n").await;

	// Push through the component, over wasi:http.
	let mut session = Session::open(&client_git).await?;
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_push(&mut session.store, session.handle, &url, false, None)
		.await?
		.map_err(|error| anyhow!("push: {error:?}"))?;
	match outcome {
		PushOutcome::Pushed(summary) => {
			assert_eq!(summary.branch, "refs/heads/main");
			assert!(!summary.forced);
		}
		other => panic!("expected a pushed branch, got {other:?}"),
	}

	// The server now has `refs/heads/main` at the client tip, with the pushed objects present.
	let server = native_repo::<H>(&server_git)?;
	assert_eq!(server.refs().resolve("refs/heads/main").await?, Some(tip));
	server.commit_tree(tip).await?;
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_sha256() {
	push_creates_remote_branch::<Sha256>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_sha1() {
	push_creates_remote_branch::<Sha1>().await.unwrap();
}

/// A non-fast-forward push without `force` is refused client-side, before anything is sent — even
/// though this harness's receive-pack runs with force on and would accept the rewrite. Proves the
/// component honours its `force` contract rather than leaning on the server to reject.
async fn push_refuses_non_fast_forward<H: HashAlgorithm>() -> Result<()> {
	// A server repo whose `main` holds one (unrelated) root commit.
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	let server_tip = commit_file(&server, "server.txt", b"server\n").await;
	let url = serve(server_git.clone(), kind_of::<H>()).await;

	// A client repo whose `main` is a *different* root — diverged, not a fast-forward of the server.
	let cli = tempfile::tempdir()?;
	let client_git = cli.path().join("client.git");
	std::fs::create_dir_all(&client_git)?;
	let client = open::<H>(&client_git);
	client.init().await?;
	commit_file(&client, "client.txt", b"client\n").await;

	// A plain push (force = false) must be rejected.
	let mut session = Session::open(&client_git).await?;
	let result = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_push(&mut session.store, session.handle, &url, false, None)
		.await?;
	assert!(
		result.is_err(),
		"expected a non-fast-forward push to be refused, got {result:?}"
	);

	// The server branch is untouched — nothing was sent.
	let server = native_repo::<H>(&server_git)?;
	assert_eq!(
		server.refs().resolve("refs/heads/main").await?,
		Some(server_tip)
	);
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_refuses_non_fast_forward_sha256() {
	push_refuses_non_fast_forward::<Sha256>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_refuses_non_fast_forward_sha1() {
	push_refuses_non_fast_forward::<Sha1>().await.unwrap();
}
