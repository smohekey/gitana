//! End-to-end `clone_url` over HTTP against a real `git http-backend` server. Proves gitana's clone
//! client interoperates with stock git: build a bare git repository, serve it behind `git
//! http-backend` (bridged through axum), and clone it with [`gitana_clone::clone_url`].
//!
//! Gated on `git http-backend` being installed; skipped otherwise.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use gitana_clone::{Anonymous, CloneError, Deepen, clone_url, fetch_url};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn git_try(dir: &Path, args: &[&str]) -> Output {
	Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.output()
		.expect("run git")
}

fn git(dir: &Path, args: &[&str]) -> String {
	let out = git_try(dir, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Whether stock `git` ships `git-http-backend`. Cached.
fn git_http_backend_available() -> bool {
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

/// Serve `project_root` over HTTP through `git http-backend`, returning the base URL. Any path under
/// the returned URL maps to `<project_root>/<path>` (e.g. `<base>/repo.git`).
async fn serve_git_http_backend(project_root: PathBuf) -> String {
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
/// body to its stdin, and translate its CGI response back to HTTP.
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
/// honouring the `Status:` (default 200) and `Content-Type:` headers `git http-backend` emits.
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

/// Build `<root>/repo.git`: a bare git repo with one commit on `main` (`a.txt` = `hello\n`). Returns
/// the `main` tip hex.
fn build_bare(root: &Path) -> String {
	let work = root.join("work");
	std::fs::create_dir_all(&work).unwrap();
	git(&work, &["init", "-q", "-b", "main", "."]);
	git(&work, &["config", "user.name", "T"]);
	git(&work, &["config", "user.email", "t@e"]);
	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	git(&work, &["add", "."]);
	git(&work, &["commit", "-qm", "first"]);
	let head = git(&work, &["rev-parse", "HEAD"]);

	let bare = root.join("repo.git");
	let out = git_try(
		Path::new("."),
		&[
			"clone",
			"--bare",
			"-q",
			work.to_str().unwrap(),
			bare.to_str().unwrap(),
		],
	);
	assert!(
		out.status.success(),
		"bare clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	head
}

#[tokio::test]
async fn clones_a_repository_over_http() {
	if !git_http_backend_available() {
		eprintln!("skipping: git http-backend is not installed");
		return;
	}
	let temp = tempfile::tempdir().unwrap();
	let head = build_bare(temp.path());
	let base = serve_git_http_backend(temp.path().to_path_buf()).await;

	let destination = temp.path().join("cloned");
	clone_url(
		&format!("{base}/repo.git"),
		&destination,
		Anonymous,
		&Deepen::default(),
	)
	.await
	.expect("clone over HTTP");

	// The working tree was populated from HEAD.
	assert_eq!(
		std::fs::read_to_string(destination.join("a.txt")).unwrap(),
		"hello\n"
	);
	// The clone is a valid git repository at the same commit — stock git reads gitana's output.
	assert_eq!(git(&destination, &["rev-parse", "HEAD"]), head);
	// The origin URL was persisted (userinfo-free).
	assert_eq!(
		git(&destination, &["config", "remote.origin.url"]),
		format!("{base}/repo.git")
	);
}

#[tokio::test]
async fn fetches_new_commits_over_http() {
	if !git_http_backend_available() {
		eprintln!("skipping: git http-backend is not installed");
		return;
	}
	let temp = tempfile::tempdir().unwrap();
	let head1 = build_bare(temp.path());
	let base = serve_git_http_backend(temp.path().to_path_buf()).await;

	let destination = temp.path().join("cloned");
	clone_url(
		&format!("{base}/repo.git"),
		&destination,
		Anonymous,
		&Deepen::default(),
	)
	.await
	.expect("clone over HTTP");

	// Advance the origin: a second commit on `main`, pushed into the served bare repo.
	let work = temp.path().join("work");
	std::fs::write(work.join("a.txt"), b"hello\nagain\n").unwrap();
	git(&work, &["commit", "-aqm", "second"]);
	let head2 = git(&work, &["rev-parse", "HEAD"]);
	let bare = temp.path().join("repo.git");
	git(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);

	// Fetch lands the new commit and advances the remote-tracking ref to head2.
	fetch_url(&format!("{base}/repo.git"), &destination, Anonymous)
		.await
		.expect("fetch over HTTP");
	assert_eq!(
		git(&destination, &["rev-parse", "refs/remotes/origin/main"]),
		head2
	);
	// The working tree and HEAD are untouched — a fetch updates tracking refs only.
	assert_eq!(
		std::fs::read_to_string(destination.join("a.txt")).unwrap(),
		"hello\n"
	);
	assert_eq!(git(&destination, &["rev-parse", "HEAD"]), head1);
}

#[tokio::test]
async fn a_missing_repository_reports_a_clone_error() {
	if !git_http_backend_available() {
		return;
	}
	let temp = tempfile::tempdir().unwrap();
	build_bare(temp.path());
	let base = serve_git_http_backend(temp.path().to_path_buf()).await;

	let error = clone_url(
		&format!("{base}/absent.git"),
		&temp.path().join("cloned"),
		Anonymous,
		&Deepen::default(),
	)
	.await
	.expect_err("cloning a missing repository fails");
	assert!(matches!(
		error,
		CloneError::Advertisement { .. } | CloneError::Clone { .. }
	));
}
