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
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	NoReplayCheck, ProtocolVersion, PushReflog, ReceiveOptions, Service, TrustContext, advertise,
	fetch, ls_refs, receive_pack, upload_pack_v0,
};
use gitana_object::{HashAlgorithm, PktLine, Sha1, Sha256, parse_pkt};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use tokio::io::AsyncWriteExt;
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
	/// The server committer line to credit push reflogs to (`Name <email> secs ±hhmm`), or `None` to
	/// write none — the default interop server has no identity, matching git's default bare server.
	reflog_committer: Option<String>,
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

/// Serve a receive-pack POST: `force` on, trust unconfigured (interop, not trust). `reflog_committer`
/// credits accepted-update push reflogs to the server identity, or `None` writes none.
async fn receive_pack_bytes<H: HashAlgorithm>(
	git_dir: &Path,
	body: &[u8],
	reflog_committer: Option<&str>,
) -> Vec<u8> {
	receive_pack(
		&open::<H>(git_dir),
		body,
		ReceiveOptions {
			force: true,
			trust: &TrustContext::none(),
			now: 0,
			nonce_ledger: &NoReplayCheck,
			reflog: reflog_committer.map(|committer| PushReflog { committer }),
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
	// git gzips a large upload-pack request (`Content-Encoding: gzip`) — e.g. a multi-round fetch whose
	// `have` batches grow big — so decompress it before the transport-agnostic handlers see raw pkt-lines.
	let body = decode_body(&headers, body);
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

/// Decompress a request body git sent with `Content-Encoding: gzip`; pass any other body through
/// unchanged. (A real Smart-HTTP server does the same before handing the pkt-line stream to upload-pack.)
fn decode_body(headers: &HeaderMap, body: Bytes) -> Bytes {
	let gzipped = headers
		.get("content-encoding")
		.and_then(|v| v.to_str().ok())
		.is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
	if !gzipped {
		return body;
	}
	use std::io::Read;
	let mut decoder = flate2::read::GzDecoder::new(&body[..]);
	let mut out = Vec::new();
	decoder.read_to_end(&mut out).expect("gunzip request body");
	Bytes::from(out)
}

/// `POST /git-receive-pack`.
async fn git_receive_pack(State(st): State<Served>, body: Bytes) -> Response {
	let committer = st.reflog_committer.as_deref();
	let out = match st.hash {
		ServerHash::Sha1 => receive_pack_bytes::<Sha1>(&st.git_dir, &body, committer).await,
		ServerHash::Sha256 => receive_pack_bytes::<Sha256>(&st.git_dir, &body, committer).await,
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
	serve(Served {
		git_dir,
		hash,
		reflog_committer: None,
	})
	.await
}

/// Like [`serve_gitana`], but crediting server-side push reflogs to `committer` (a git reflog
/// committer line, `Name <email> secs ±hhmm`) — for the receive-pack reflog oracle.
pub async fn serve_gitana_with_reflog(
	git_dir: PathBuf,
	hash: ServerHash,
	committer: String,
) -> String {
	serve(Served {
		git_dir,
		hash,
		reflog_committer: Some(committer),
	})
	.await
}

/// Bind an ephemeral loopback port and serve `state`'s repo; returns the base URL.
async fn serve(state: Served) -> String {
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-upload-pack", post(upload_pack))
		.route("/git-receive-pack", post(git_receive_pack))
		.with_state(state);
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

// --- Direction B: serve a real git repo via `git http-backend` (CGI) ----------------------------

/// Serve `project_root` (holding one or more bare repos) over Smart-HTTP by bridging `git
/// http-backend` (a CGI program) behind axum. Returns the base URL; a repo at `<root>/repo.git` is
/// reached at `<url>/repo.git`. Lets the gitana `gta` client talk to a real git server.
pub async fn serve_git_http_backend(project_root: PathBuf) -> String {
	let app = Router::new()
		.fallback(git_http_backend_cgi)
		.with_state(project_root);
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	format!("http://{addr}")
}

/// Bridge one HTTP request to `git http-backend`: set the CGI environment from the request, pipe the
/// body to its stdin, and translate its CGI response (headers, blank line, body) back to HTTP.
async fn git_http_backend_cgi(
	State(root): State<PathBuf>,
	method: Method,
	uri: Uri,
	headers: HeaderMap,
	body: Bytes,
) -> Response {
	let content_type = headers
		.get(CONTENT_TYPE)
		.and_then(|v| v.to_str().ok())
		.unwrap_or("")
		.to_owned();
	let mut child = tokio::process::Command::new("git")
		.arg("http-backend")
		.env_clear()
		// `git` still needs PATH (to find its subcommands) after env_clear.
		.env("PATH", std::env::var("PATH").unwrap_or_default())
		.env("GIT_PROJECT_ROOT", &root)
		.env("GIT_HTTP_EXPORT_ALL", "1")
		.env("PATH_INFO", uri.path())
		.env("QUERY_STRING", uri.query().unwrap_or(""))
		.env("REQUEST_METHOD", method.as_str())
		.env("CONTENT_TYPE", &content_type)
		.env("CONTENT_LENGTH", body.len().to_string())
		.env("REMOTE_ADDR", "127.0.0.1")
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.expect("spawn git http-backend");
	child
		.stdin
		.take()
		.unwrap()
		.write_all(&body)
		.await
		.expect("write cgi body");
	let out = child.wait_with_output().await.expect("git http-backend");
	assert!(
		out.status.success(),
		"git http-backend failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	parse_cgi(&out.stdout)
}

/// Split a CGI response (`Header: value` lines, a blank line, then the body) into an HTTP response,
/// honouring the `Status:` (default 200) and `Content-Type:` headers git http-backend emits.
fn parse_cgi(raw: &[u8]) -> Response {
	let split = raw
		.windows(4)
		.position(|w| w == b"\r\n\r\n")
		.map(|i| (i, i + 4))
		.or_else(|| {
			raw
				.windows(2)
				.position(|w| w == b"\n\n")
				.map(|i| (i, i + 2))
		})
		.expect("cgi header/body separator");
	let (head, body) = (&raw[..split.0], &raw[split.1..]);
	let head = String::from_utf8_lossy(head);

	let mut status = StatusCode::OK;
	let mut content_type: Option<String> = None;
	for line in head.lines() {
		if let Some(value) = line.strip_prefix("Status:") {
			if let Some(code) = value
				.trim()
				.split(' ')
				.next()
				.and_then(|c| c.parse::<u16>().ok())
			{
				status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
			}
		} else if let Some(value) = line.strip_prefix("Content-Type:") {
			content_type = Some(value.trim().to_owned());
		}
	}
	let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_owned());
	(
		status,
		[(CONTENT_TYPE, content_type)],
		Bytes::copy_from_slice(body),
	)
		.into_response()
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
/// Direction B. Checked by looking for the actual `git-http-backend` binary in git's exec-path —
/// running `git http-backend --help` is no good, because a missing subcommand still prints a message
/// containing "http-backend" ("git: 'http-backend' is not a git command").
pub fn git_http_backend_available() -> bool {
	static AVAILABLE: OnceLock<bool> = OnceLock::new();
	*AVAILABLE.get_or_init(|| {
		Command::new("git")
			.arg("--exec-path")
			.output()
			.ok()
			.filter(|o| o.status.success())
			.and_then(|o| String::from_utf8(o.stdout).ok())
			.map(|exec_path| {
				Path::new(exec_path.trim())
					.join(format!("git-http-backend{}", std::env::consts::EXE_SUFFIX))
					.exists()
			})
			.unwrap_or(false)
	})
}
