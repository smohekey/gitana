//! End-to-end proof of the in-component remote porcelain over `wasi:http`.
//!
//! A loopback axum server serves gitana's OWN Smart-HTTP handlers (`advertise` / `upload_pack_v0`)
//! over `http://127.0.0.1:<port>` against a temp server repo. The component — instantiated with **no
//! preopens**, its only network authority the host-granted `wasi:http/outgoing-handler` — is granted
//! an empty client git dir and asked to `fetch` from that URL. The whole transport path runs in the
//! reactor: the advertisement GET and the pack POST both flow through the in-guest `WasiHttpTransport`,
//! blocking inline on `wasi:io` pollables under the sync-export `block_on`. We check the tracking ref
//! advanced and the pack landed on disk — in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{ProtocolVersion, Service, advertise, upload_pack_v0};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use tokio::net::TcpListener;

use self::support::{Session, native_repo};

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

/// Start the server over `git_dir` on an ephemeral port; the listener is bound before this returns,
/// so there is no startup race with the fetch below.
async fn serve(git_dir: PathBuf, kind: HashKind) -> String {
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
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
