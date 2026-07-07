//! Shared helpers for the stock-`git` ↔ gitana interop tests.
//!
//! Direction A (this slice) serves gitana's own Smart-HTTP handlers behind a loopback axum server
//! ([`serve_gitana`]) with a protocol-v0/v2 dispatcher, so real `git` can clone/fetch/push against
//! gitana. Direction B (a later slice) will add a `git http-backend` CGI bridge. Plus the shared
//! real-`git` / `gta` runners, a unique temp dir, and cached capability probes.
//!
//! Each test binary pulls this in with `mod support;`; not every binary uses every helper.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	NoReplayCheck, ProtocolVersion, ReceiveOptions, Service, TrustContext, advertise, fetch, ls_refs,
	receive_pack, upload_pack_v0,
};
use gitana_object::{HashAlgorithm, PktLine, Sha1, Sha256, parse_pkt};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use tokio::net::TcpListener;

// --- repo open ----------------------------------------------------------------------------------

/// Open a gitana repo (hash `H`) fresh from its git dir. Handlers re-open per request so pushes
/// persist on disk between requests; a single test never touches its repo concurrently.
pub fn open<H: HashAlgorithm>(git_dir: &Path) -> Repository<LocalFileStore, H> {
	Repository::new(ObjectStore::<_, H>::new(LocalFileStore::from_dir(
		cap_std::fs::Dir::open_ambient_dir(git_dir, cap_std::ambient_authority()).unwrap(),
	)))
}

// --- Direction A: serve gitana behind axum ------------------------------------------------------

/// Which hash a served gitana repo uses. Axum handlers must be non-generic (a concrete `Handler`),
/// so the served hash is a value in the state and each handler branches to the concrete
/// monomorphisation — rather than a generic handler axum can't prove `Send` for an abstract `H`.
#[derive(Clone, Copy)]
pub enum ServerHash {
	Sha1,
	Sha256,
}

/// The axum state for a served gitana repo.
#[derive(Clone)]
struct Served {
	git_dir: PathBuf,
	hash: ServerHash,
}

/// The `Git-Protocol` version the request asks for (git repeats the header on every request).
fn protocol(headers: &HeaderMap) -> ProtocolVersion {
	ProtocolVersion::from_header(headers.get("Git-Protocol").and_then(|v| v.to_str().ok()))
}

/// Whether the request body's first pkt-line is `command=<name>` (a protocol-v2 command).
fn first_command_is(body: &[u8], name: &str) -> bool {
	match parse_pkt(body) {
		Ok((PktLine::Data(data), _)) => data.trim_ascii() == format!("command={name}").as_bytes(),
		_ => false,
	}
}

/// Advertise `git_dir`'s refs for `service` under `version` (no push-cert nonce — the interop matrix
/// pushes unsigned).
async fn advertise_refs<H: HashAlgorithm>(
	git_dir: &Path,
	service: Service,
	version: ProtocolVersion,
) -> Vec<u8> {
	advertise(&open::<H>(git_dir), service, version, None)
		.await
		.expect("advertise")
}

/// Serve a v0/v2 upload-pack POST: dispatch on version and (v2) the command pkt-line.
async fn upload_pack_bytes<H: HashAlgorithm>(
	git_dir: &Path,
	version: ProtocolVersion,
	body: &[u8],
) -> Vec<u8> {
	let repo = open::<H>(git_dir);
	match version {
		ProtocolVersion::V2 if first_command_is(body, "ls-refs") => {
			ls_refs(&repo, body).await.expect("ls-refs")
		}
		ProtocolVersion::V2 if first_command_is(body, "fetch") => {
			fetch(&repo, body).await.expect("fetch")
		}
		ProtocolVersion::V2 => panic!("unrecognised v2 upload-pack command"),
		ProtocolVersion::V0 => upload_pack_v0(&repo, body).await.expect("upload-pack"),
	}
}

/// Serve a receive-pack POST: `force` on, trust unconfigured (interop, not trust).
async fn receive_pack_bytes<H: HashAlgorithm>(git_dir: &Path, body: &[u8]) -> Vec<u8> {
	receive_pack(
		&open::<H>(git_dir),
		body,
		ReceiveOptions {
			force: true,
			trust: &TrustContext::none(),
			now: 0,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive-pack")
	.report
}

/// `GET /info/refs?service=…`. Upload-pack honours the client's protocol version; receive-pack is
/// always v0 (git never negotiates v2 for push).
async fn info_refs(
	State(st): State<Served>,
	headers: HeaderMap,
	RawQuery(query): RawQuery,
) -> Response {
	let raw = query.unwrap_or_default();
	let service = Service::parse(raw.strip_prefix("service=").unwrap_or(&raw)).expect("service");
	let version = match service {
		Service::ReceivePack => ProtocolVersion::V0,
		Service::UploadPack => protocol(&headers),
	};
	let body = match st.hash {
		ServerHash::Sha1 => advertise_refs::<Sha1>(&st.git_dir, service, version).await,
		ServerHash::Sha256 => advertise_refs::<Sha256>(&st.git_dir, service, version).await,
	};
	(
		[(CONTENT_TYPE, service.advertisement_content_type())],
		Bytes::from(body),
	)
		.into_response()
}

/// `POST /git-upload-pack`.
async fn upload_pack(State(st): State<Served>, headers: HeaderMap, body: Bytes) -> Response {
	let version = protocol(&headers);
	let out = match st.hash {
		ServerHash::Sha1 => upload_pack_bytes::<Sha1>(&st.git_dir, version, &body).await,
		ServerHash::Sha256 => upload_pack_bytes::<Sha256>(&st.git_dir, version, &body).await,
	};
	(
		[(CONTENT_TYPE, Service::UploadPack.result_content_type())],
		Bytes::from(out),
	)
		.into_response()
}

/// `POST /git-receive-pack`.
async fn git_receive_pack(State(st): State<Served>, body: Bytes) -> Response {
	let out = match st.hash {
		ServerHash::Sha1 => receive_pack_bytes::<Sha1>(&st.git_dir, &body).await,
		ServerHash::Sha256 => receive_pack_bytes::<Sha256>(&st.git_dir, &body).await,
	};
	(
		[(CONTENT_TYPE, Service::ReceivePack.result_content_type())],
		Bytes::from(out),
	)
		.into_response()
}

/// Serve gitana's `git_dir` (hash `hash`) on an ephemeral loopback port; returns the base URL. The
/// listener is bound before returning, so there is no startup race.
pub async fn serve_gitana(git_dir: PathBuf, hash: ServerHash) -> String {
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
		.route("/git-receive-pack", post(git_receive_pack))
		.with_state(Served { git_dir, hash });
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

// --- external-command runners -------------------------------------------------------------------

/// Run `git -C dir <args>`, returning the raw output (no success assertion).
pub fn git_try(dir: &Path, args: &[&str]) -> Output {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git")
}

/// Run `git -C dir <args>`, asserting success and returning trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_try(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Run the `gta` binary with `args` (subprocess), off the runtime so a server task keeps serving.
pub async fn gta(args: &[&str]) -> Output {
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

/// Assert a `gta` invocation succeeded (naming `what` in the failure message).
pub fn gta_ok(out: &Output, what: &str) {
	assert!(
		out.status.success(),
		"gta {what} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// The trimmed stdout of a successful `gta` invocation.
pub fn gta_stdout(out: &Output, what: &str) -> String {
	gta_ok(out, what);
	String::from_utf8(out.stdout.clone())
		.unwrap()
		.trim()
		.to_owned()
}

// --- temp dirs + capability probes --------------------------------------------------------------

/// A fresh, unique temp dir named for `tag` (process-id + sequence), cleared if stale.
pub fn unique_tmp(tag: &str) -> PathBuf {
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Whether stock `git` can create a SHA-256 repository (git ≥ 2.29). Cached.
pub fn git_supports_sha256() -> bool {
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-sha256");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}

/// Whether `git http-backend` is present (absent on some minimal installs). Cached. Used by
/// Direction B. `--help` exits non-zero but prints usage, so we check it spawned and mentioned itself.
pub fn git_http_backend_available() -> bool {
	static AVAILABLE: OnceLock<bool> = OnceLock::new();
	*AVAILABLE.get_or_init(|| {
		Command::new("git")
			.args(["http-backend", "--help"])
			.output()
			.map(|o| {
				let text = String::from_utf8_lossy(&o.stderr) + String::from_utf8_lossy(&o.stdout);
				text.contains("http-backend")
			})
			.unwrap_or(false)
	})
}
