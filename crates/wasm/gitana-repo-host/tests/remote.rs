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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	NoReplayCheck, ProtocolVersion, ReceiveOptions, Service, TrustContext, advertise, receive_pack,
	upload_pack_v0,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repo_host::exports::gitana::repo::porcelain::PushOutcome;
use gitana_repo_host::{
	StoreFileCredentials, grant_dir, instantiate_component, store, store_with_credentials,
};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use tokio::net::TcpListener;

use self::support::{Session, native_repo, shared};

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
		nonce_ledger: &NoReplayCheck,
		reflog: None,
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
	serve_gated(git_dir, kind, None).await
}

/// Like [`serve`], but gating every request behind `401 WWW-Authenticate: Basic` unless it presents the
/// `user:pass` Basic credential — the server side of the in-component credential flow.
async fn serve_basic_auth(git_dir: PathBuf, kind: HashKind, user: &str, pass: &str) -> String {
	serve_gated(git_dir, kind, Some((user.to_owned(), pass.to_owned()))).await
}

/// Bind an ephemeral loopback port and serve `git_dir`, optionally behind a Basic-auth gate; returns
/// the base URL.
async fn serve_gated(
	git_dir: PathBuf,
	kind: HashKind,
	basic_auth: Option<(String, String)>,
) -> String {
	let expected = basic_auth.map(|(user, pass)| {
		format!(
			"Basic {}",
			base64_encode(format!("{user}:{pass}").as_bytes())
		)
	});
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
		.route("/git-receive-pack", post(receive_pack_srv))
		.layer(axum::middleware::from_fn(move |req, next| {
			basic_auth_gate(expected.clone(), req, next)
		}))
		.with_state(ServerState { git_dir, kind });
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

/// Reject a request lacking the `expected` `Authorization` header with `401 WWW-Authenticate: Basic`;
/// `None` means the server is anonymous and every request passes — mirrors an authenticated git
/// http-backend.
async fn basic_auth_gate(
	expected: Option<String>,
	req: axum::extract::Request,
	next: axum::middleware::Next,
) -> Response {
	if let Some(expected) = expected {
		let presented = req
			.headers()
			.get("authorization")
			.and_then(|value| value.to_str().ok());
		if presented != Some(expected.as_str()) {
			return (
				StatusCode::UNAUTHORIZED,
				[("WWW-Authenticate", "Basic realm=\"gitana\"")],
				"authentication required",
			)
				.into_response();
		}
	}
	next.run(req).await
}

/// Standard base64 (RFC 4648) of `input` — the server side of the Basic-auth oracle, so the harness
/// needs no base64 dependency of its own.
fn base64_encode(input: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::new();
	for chunk in input.chunks(3) {
		let b = [
			chunk[0],
			*chunk.get(1).unwrap_or(&0),
			*chunk.get(2).unwrap_or(&0),
		];
		let group = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
		out.push(ALPHABET[(group >> 18) as usize & 0x3f] as char);
		out.push(ALPHABET[(group >> 12) as usize & 0x3f] as char);
		out.push(if chunk.len() > 1 {
			ALPHABET[(group >> 6) as usize & 0x3f] as char
		} else {
			'='
		});
		out.push(if chunk.len() > 2 {
			ALPHABET[group as usize & 0x3f] as char
		} else {
			'='
		});
	}
	out
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
	let (engine, component) = shared();
	let mut store = store(engine);
	let repo = instantiate_component(engine, &mut store, component).await?;
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

// --- HTTP credentials: the host answers the guest's `credentials` import ------------------------

/// The store line granting `alice:s3cr3t` for the ephemeral host `url` names (`http://…@host`), plus the
/// store-file path — written under `dir` so it lives as long as the fixture.
fn seed_store(dir: &Path, url: &str) -> Result<PathBuf> {
	let host = url
		.strip_prefix("http://")
		.ok_or_else(|| anyhow!("http url"))?;
	let store = dir.join("credential-store");
	std::fs::write(&store, format!("http://alice:s3cr3t@{host}\n"))?;
	Ok(store)
}

/// Against a `401`-gated server, the component authenticates the fetch with the credential the host
/// resolves from its store file — proving the whole path: `AuthTransport` unauth-first → `401` → the WIT
/// `fill` import → the host's `StoreFileCredentials` → retry with `Authorization: Basic` → `200`.
async fn fetch_authenticates_via_host_credentials<H: HashAlgorithm>() -> Result<()> {
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	let tip = commit_file(&server, "hello.txt", b"world\n").await;
	let url = serve_basic_auth(server_git, kind_of::<H>(), "alice", "s3cr3t").await;
	let store = seed_store(srv.path(), &url)?;

	// An empty client repo of the same object format.
	let cli = tempfile::tempdir()?;
	let client_git = cli.path().join("client.git");
	std::fs::create_dir_all(&client_git)?;
	open::<H>(&client_git).init().await?;

	// Fetch through the component, with the host answering credentials from the store file.
	let mut session =
		Session::open_with_credentials(&client_git, Box::new(StoreFileCredentials::new(&store)))
			.await?;
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut session.store, session.handle, &url)
		.await?
		.map_err(|error| anyhow!("fetch: {error:?}"))?;

	// The authenticated fetch advanced the tracking ref and the objects landed.
	assert!(
		outcome
			.updated
			.iter()
			.any(|r| r.name == "refs/remotes/origin/main" && r.id == tip.to_hex()),
		"expected refs/remotes/origin/main at {}, got {:?}",
		tip.to_hex(),
		outcome.updated
	);
	let client = native_repo::<H>(&client_git)?;
	assert_eq!(
		client.refs().resolve("refs/remotes/origin/main").await?,
		Some(tip)
	);
	client.commit_tree(tip).await?;

	// The server accepted the credential, so `approve` ran and de-duped rather than appended: the store
	// still holds exactly the one entry (proof the report path reached the host, not just `fill`).
	assert_eq!(
		std::fs::read_to_string(&store)?.lines().count(),
		1,
		"approve should leave a single de-duped store entry"
	);
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_authenticates_sha256() {
	fetch_authenticates_via_host_credentials::<Sha256>()
		.await
		.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_authenticates_sha1() {
	fetch_authenticates_via_host_credentials::<Sha1>()
		.await
		.unwrap();
}

/// With no credential source granted (the default anonymous `State`), a `401`-gated server's challenge
/// stands: the component sends nothing to authenticate with, and the fetch fails rather than succeeding.
async fn fetch_without_credential_leaves_401_standing<H: HashAlgorithm>() -> Result<()> {
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	commit_file(&server, "hello.txt", b"world\n").await;
	let url = serve_basic_auth(server_git, kind_of::<H>(), "alice", "s3cr3t").await;

	let cli = tempfile::tempdir()?;
	let client_git = cli.path().join("client.git");
	std::fs::create_dir_all(&client_git)?;
	open::<H>(&client_git).init().await?;

	// The default session grants no credential source — every `fill` yields nothing.
	let mut session = Session::open(&client_git).await?;
	let result = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut session.store, session.handle, &url)
		.await?;
	assert!(
		result.is_err(),
		"expected the 401 to stand without a credential, got {result:?}"
	);
	// Nothing was fetched: the tracking ref was never created.
	let client = native_repo::<H>(&client_git)?;
	assert_eq!(
		client.refs().resolve("refs/remotes/origin/main").await?,
		None
	);
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_without_credential_leaves_401_standing_sha256() {
	fetch_without_credential_leaves_401_standing::<Sha256>()
		.await
		.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_without_credential_leaves_401_standing_sha1() {
	fetch_without_credential_leaves_401_standing::<Sha1>()
		.await
		.unwrap();
}

/// Clone exercises the credential path through both entry points — the advertisement `GET`
/// (`clone_negotiate`) and the pack `POST` (`clone`) — so a `401`-gated server is cloned end to end
/// with the host-resolved credential: objects land, `HEAD` resolves, the working tree materialises.
async fn clone_authenticates_via_host_credentials<H: HashAlgorithm>() -> Result<()> {
	let srv = tempfile::tempdir()?;
	let server_git = srv.path().join("srv.git");
	std::fs::create_dir_all(&server_git)?;
	let server = open::<H>(&server_git);
	server.init().await?;
	let tip = commit_file(&server, "hello.txt", b"world\n").await;
	let url = serve_basic_auth(server_git, kind_of::<H>(), "alice", "s3cr3t").await;
	let store = seed_store(srv.path(), &url)?;

	// An empty client checkout: the working dir and its `.git`, both empty.
	let cli = tempfile::tempdir()?;
	let work = cli.path().join("checkout");
	let git = work.join(".git");
	std::fs::create_dir_all(&git)?;

	// Clone through the component, the host answering credentials from the store file.
	let (engine, component) = shared();
	let mut store_handle =
		store_with_credentials(engine, Box::new(StoreFileCredentials::new(&store)));
	let repo = instantiate_component(engine, &mut store_handle, component).await?;
	let git_desc = grant_dir(&mut store_handle, &git)?;
	let work_desc = grant_dir(&mut store_handle, &work)?;
	repo
		.gitana_repo_porcelain()
		.repository()
		.call_clone(&mut store_handle, git_desc, work_desc, &url)
		.await?
		.map_err(|error| anyhow!("clone: {error:?}"))?;

	// The authenticated clone populated the checkout.
	let client = native_repo::<H>(&git)?;
	assert_eq!(client.refs().resolve_head().await?, Some(tip));
	client.commit_tree(tip).await?;
	assert_eq!(std::fs::read(work.join("hello.txt"))?, b"world\n");
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_authenticates_sha256() {
	clone_authenticates_via_host_credentials::<Sha256>()
		.await
		.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_authenticates_sha1() {
	clone_authenticates_via_host_credentials::<Sha1>()
		.await
		.unwrap();
}
