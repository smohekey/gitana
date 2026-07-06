//! Capstone end-to-end validation (`docs/hlds/secure-git-trust-signing.md`, step 8, matrix row 3):
//! a real, stock `git push --signed` into gitana's own `receive_pack`, over gitana's own HMAC
//! push-cert nonce. Unlike the in-crate `enforce` wire tests (which build requests by hand) and the
//! captured-fixture cert test (which only checks the signature), this drives the *whole* assembly
//! against the real client: nonce advertise/echo → pushee → signed commands → certificate signature
//! → newly-introduced object signatures, all under a `require` trust root.
//!
//! Uses SHA-1 (stock git's default); skips where `ssh-keygen` or an ssh-signing-capable git is
//! absent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::{
	NoReplayCheck, ProtocolVersion, ReceiveOptions, Service, TrustContext, advertise, make_nonce,
	receive_pack,
};
use gitana_object::{Commit, ObjectId, ObjectKind, Sha1, TreeEntry, encode_commit, encode_tree};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use tempfile::TempDir;
use tokio::net::TcpListener;

/// The server's push-nonce HMAC secret and the identity a certificate is bound to.
const SECRET: &[u8] = b"real-git-e2e-secret";
const REPO_ID: &str = "e2e/repo";
/// A fixed clock: the advertised nonce and `receive_pack` share it, so the freshness check is
/// deterministic (git only echoes the nonce bytes; it never inspects the timestamp).
const NOW: u64 = 1_700_000_000;
const RANDOM: &[u8] = b"\x01\x02\x03\x04";
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

// --- the loopback gitana receive-pack server ----------------------------------------------------

#[derive(Clone)]
struct AppState {
	git_dir: PathBuf,
	/// The URL clients push to — a certificate's `pushee` must equal it.
	pushee: String,
}

fn open(git_dir: &Path) -> Repository<LocalFileStore, Sha1> {
	Repository::new(ObjectStore::<_, Sha1>::new(LocalFileStore::from_dir(
		cap_std::fs::Dir::open_ambient_dir(git_dir, cap_std::ambient_authority()).unwrap(),
	)))
}

fn service_name(service: Service) -> &'static str {
	match service {
		Service::ReceivePack => "git-receive-pack",
		Service::UploadPack => "git-upload-pack",
	}
}

/// `GET /info/refs?service=…` — the v0 advertisement, with a real gitana HMAC nonce on receive-pack
/// so `git push --signed` has something to sign. Stock git requires the smart-http content type.
async fn info_refs(State(state): State<AppState>, RawQuery(query): RawQuery) -> impl IntoResponse {
	let raw = query.unwrap_or_default();
	let service = Service::parse(raw.strip_prefix("service=").unwrap_or(&raw)).expect("service");
	let nonce =
		matches!(service, Service::ReceivePack).then(|| make_nonce(SECRET, REPO_ID, NOW, RANDOM));
	let body = advertise(
		&open(&state.git_dir),
		service,
		ProtocolVersion::V0,
		nonce.as_deref(),
	)
	.await
	.expect("advertise");
	let content_type = format!("application/x-{}-advertisement", service_name(service));
	([(CONTENT_TYPE, content_type)], Bytes::from(body))
}

/// `POST /git-receive-pack` — the signed push, verified against a real trust context (secret, repo
/// id, pushee) at the fixed clock. The handler re-opens the repo so accepted writes land on disk.
async fn git_receive_pack(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
	let repo = open(&state.git_dir);
	let trust = TrustContext {
		nonce_secret: SECRET.to_vec(),
		repo_id: REPO_ID.to_owned(),
		pushee: state.pushee.clone(),
		nonce_slop_secs: 3600,
	};
	let outcome = receive_pack(
		&repo,
		&body,
		ReceiveOptions {
			force: false,
			trust: &trust,
			now: NOW,
			nonce_ledger: &NoReplayCheck,
		},
	)
	.await
	.expect("receive-pack");
	(
		[(CONTENT_TYPE, "application/x-git-receive-pack-result")],
		Bytes::from(outcome.report),
	)
}

/// Serve `git_dir` on an ephemeral port; returns the base URL (also used as the expected `pushee`).
async fn serve(git_dir: PathBuf) -> String {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let url = format!("http://{}", listener.local_addr().expect("addr"));
	let app = Router::new()
		.route("/info/refs", get(info_refs))
		.route("/git-receive-pack", post(git_receive_pack))
		.with_state(AppState {
			git_dir,
			// git normalises a path-less remote URL to a trailing slash in the certificate `pushee`.
			pushee: format!("{url}/"),
		});
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("serve");
	});
	url
}

// --- trust-root install (a signed bootstrap enrolling `publine`, policy require) -----------------

async fn install_trust_root(git_dir: &Path, publine: &str, keyfile: &Path) {
	let repo = open(git_dir);
	let trust_json = format!("{{\"version\":1,\"policy\":\"require\",\"keys\":[\"{publine}\"]}}");
	let blob = repo
		.objects()
		.write_object(ObjectKind::Blob, trust_json.as_bytes())
		.await
		.unwrap();
	let tree = encode_tree::<Sha1>(&[TreeEntry {
		mode: "100644".to_owned(),
		name: "trust.json".to_owned(),
		id: blob,
	}]);
	let tree_id = repo
		.objects()
		.write_object(ObjectKind::Tree, &tree)
		.await
		.unwrap();
	let mut commit = Commit::<Sha1> {
		tree: tree_id,
		parents: Vec::new(),
		author: WHO.to_owned(),
		committer: WHO.to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: "gitana trust: bootstrap\n".to_owned(),
	};
	// Sign exactly the bytes git signs (the object with no gpgsig header), then fold the armor back in.
	commit.signature = Some(ssh_sign(keyfile, &encode_commit(&commit)));
	let commit_id = repo
		.objects()
		.write_object(ObjectKind::Commit, &encode_commit(&commit))
		.await
		.unwrap();
	repo
		.refs()
		.update_ref("refs/gitana/trust", commit_id, None)
		.await
		.unwrap();
}

// --- external-command helpers -------------------------------------------------------------------

/// SSH-sign `payload` in git's `git` namespace with `keyfile`; returns the armor with no trailing
/// newline (the form gitana stores).
fn ssh_sign(keyfile: &Path, payload: &[u8]) -> String {
	let mut child = Command::new("ssh-keygen")
		.args(["-Y", "sign", "-n", "git", "-f"])
		.arg(keyfile)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn ssh-keygen");
	child.stdin.take().unwrap().write_all(payload).unwrap();
	let out = child.wait_with_output().unwrap();
	assert!(
		out.status.success(),
		"ssh-keygen sign failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
}

/// Run `git -C dir <args>`, returning the raw output (no success assertion).
fn git_try(dir: &Path, args: &[&str]) -> Output {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git")
}

/// Run `git -C dir <args>`, asserting success and returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_try(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Init a SHA-1 client repo signed by `keyfile`, with one signed commit of `file`. Returns the dir.
fn signed_client(root: &Path, keyfile: &Path) -> PathBuf {
	let dir = root.join("client");
	std::fs::create_dir_all(&dir).unwrap();
	assert!(
		Command::new("git")
			.args(["init", "--object-format=sha1", "-q"])
			.arg(&dir)
			.status()
			.unwrap()
			.success()
	);
	git(&dir, &["config", "user.name", "A U Thor"]);
	git(&dir, &["config", "user.email", "a@example.com"]);
	git(&dir, &["config", "gpg.format", "ssh"]);
	git(
		&dir,
		&["config", "user.signingkey", keyfile.to_str().unwrap()],
	);
	std::fs::write(dir.join("hello.txt"), b"world\n").unwrap();
	git(&dir, &["add", "hello.txt"]);
	git(&dir, &["commit", "-S", "-m", "signed"]);
	dir
}

// --- the round-trip -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_git_push_signed_into_gitana_receive_pack() {
	if skip() {
		return;
	}
	let tmp = TempDir::new().unwrap();
	let keyfile = tmp.path().join("id");
	generate_key(&keyfile);
	let publine = std::fs::read_to_string(keyfile.with_extension("pub"))
		.unwrap()
		.trim()
		.to_owned();

	// A gitana SHA-1 server repo with a signed `require` trust root enrolling the key.
	let server_dir = tmp.path().join("srv.git");
	std::fs::create_dir_all(&server_dir).unwrap();
	open(&server_dir).init().await.unwrap();
	install_trust_root(&server_dir, &publine, &keyfile).await;
	let url = serve(server_dir.clone()).await;

	// A stock-git client with one SSH-signed commit; push it signed.
	let client = signed_client(tmp.path(), &keyfile);
	git(&client, &["remote", "add", "origin", &url]);
	let head = git(&client, &["rev-parse", "HEAD"]);
	let push = git_try(
		&client,
		&[
			"-c",
			"protocol.version=0",
			"push",
			"--signed",
			"origin",
			"HEAD:refs/heads/main",
		],
	);
	assert!(
		push.status.success(),
		"signed push rejected: {}",
		String::from_utf8_lossy(&push.stderr)
	);

	// The server accepted it: refs/heads/main now points at the client's signed commit.
	let landed = open(&server_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap();
	assert_eq!(landed, Some(ObjectId::<Sha1>::from_hex(&head).unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_git_unsigned_push_is_rejected_under_require() {
	if skip() {
		return;
	}
	let tmp = TempDir::new().unwrap();
	let keyfile = tmp.path().join("id");
	generate_key(&keyfile);
	let publine = std::fs::read_to_string(keyfile.with_extension("pub"))
		.unwrap()
		.trim()
		.to_owned();

	let server_dir = tmp.path().join("srv.git");
	std::fs::create_dir_all(&server_dir).unwrap();
	open(&server_dir).init().await.unwrap();
	install_trust_root(&server_dir, &publine, &keyfile).await;
	let url = serve(server_dir.clone()).await;

	let client = signed_client(tmp.path(), &keyfile);
	git(&client, &["remote", "add", "origin", &url]);
	// A plain (unsigned) push under `require`: no certificate, so the server refuses it.
	let push = git_try(
		&client,
		&[
			"-c",
			"protocol.version=0",
			"push",
			"origin",
			"HEAD:refs/heads/main",
		],
	);
	assert!(
		!push.status.success(),
		"an unsigned push must be rejected under require"
	);
	let landed = open(&server_dir)
		.refs()
		.resolve("refs/heads/main")
		.await
		.unwrap();
	assert_eq!(landed, None, "the protected ref must not have moved");
}

// --- skip probes --------------------------------------------------------------------------------

fn generate_key(keyfile: &Path) {
	assert!(
		Command::new("ssh-keygen")
			.args(["-t", "ed25519", "-N", "", "-C", "test", "-q", "-f"])
			.arg(keyfile)
			.status()
			.unwrap()
			.success(),
		"ssh-keygen keygen failed"
	);
}

fn skip() -> bool {
	if !have_ssh_keygen() {
		eprintln!("skipping: ssh-keygen not available");
		return true;
	}
	if !git_supports_ssh_signing() {
		eprintln!("skipping: git without ssh signing / signed push");
		return true;
	}
	false
}

fn have_ssh_keygen() -> bool {
	Command::new("ssh-keygen").arg("-?").output().is_ok()
}

/// Whether git can produce an SSH-signed commit (git >= 2.34 with `gpg.format=ssh`). Cached.
fn git_supports_ssh_signing() -> bool {
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let Ok(probe) = TempDir::new() else {
			return false;
		};
		let dir = probe.path().join("p");
		let key = probe.path().join("k");
		if std::fs::create_dir_all(&dir).is_err() {
			return false;
		}
		if !Command::new("ssh-keygen")
			.args(["-t", "ed25519", "-N", "", "-q", "-f"])
			.arg(&key)
			.status()
			.map(|s| s.success())
			.unwrap_or(false)
		{
			return false;
		}
		if !Command::new("git")
			.args(["init", "--object-format=sha1", "-q"])
			.arg(&dir)
			.status()
			.map(|s| s.success())
			.unwrap_or(false)
		{
			return false;
		}
		for kv in [
			("user.name", "P"),
			("user.email", "p@x"),
			("gpg.format", "ssh"),
		] {
			let _ = git_try(&dir, &["config", kv.0, kv.1]);
		}
		let _ = git_try(&dir, &["config", "user.signingkey", key.to_str().unwrap()]);
		git_try(&dir, &["commit", "-S", "--allow-empty", "-m", "x"])
			.status
			.success()
	})
}
