//! End-to-end proof of the in-component remote porcelain over the host `ssh-transport` capability:
//! `fetch`, `clone`, `push`.
//!
//! The component cannot spawn `ssh`, so the **host** answers its `ssh-transport` import. This harness
//! plugs in a fake provider that — exactly like the native `git_ssh_*` fake `ssh` — ignores the host and
//! runs the requested git service (`git-upload-pack` / `git-receive-pack`) locally against a **stock
//! `git`** repository. So this is a real interop check: the component's SSH *client* (the shared
//! `PackConnection` / `SshPackFetcher` negotiation, driven over the host-bridged `wasi:io` streams)
//! against stock `git-upload-pack` / `git-receive-pack`, over the plain pkt-line framing SSH uses. The
//! component is instantiated with **no preopens** and no network authority — its only means of reaching
//! the remote is the granted SSH capability.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, anyhow};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_repo_host::exports::gitana::repo::porcelain::PushOutcome;
use gitana_repo_host::{
	HostSshProvider, StoreFileCredentials, grant_dir, instantiate_component, store_with,
	store_with_ssh,
};

use self::support::{Session, native_repo, shared};

// --- the fake ssh provider ----------------------------------------------------------------------

/// A [`HostSshProvider`] that ignores the connection target and runs the requested git service locally
/// against `path` — the host side of the native `git_ssh_*` fake `ssh`. The component asks for
/// `git-upload-pack` / `git-receive-pack`; the provider spawns exactly that against the server repo, so
/// the component negotiates with stock git over the bridged streams.
struct FakeSsh;

impl HostSshProvider for FakeSsh {
	fn open(
		&self,
		_host: &str,
		_port: Option<u16>,
		_user: Option<&str>,
		remote_command: &str,
	) -> Result<tokio::process::Command, String> {
		// Simulate the remote login shell: run the host-built `git-<service> '<path>'` via `sh -c`, as a
		// real ssh remote's shell would (and as the native git_ssh_* fake ssh does with `eval`). This
		// exercises the host's single-quoting — a path with spaces or an apostrophe round-trips intact. The
		// host applies the stdio, kill-on-drop, and `GIT_PROTOCOL` scrub and spawns it.
		let mut command = tokio::process::Command::new("sh");
		command.arg("-c").arg(remote_command);
		Ok(command)
	}
}

// --- stock-git helpers --------------------------------------------------------------------------

/// A unique temp directory under the system temp dir (the tests run concurrently).
fn unique_tmp(tag: &str) -> PathBuf {
	static COUNTER: AtomicU32 = AtomicU32::new(0);
	let n = COUNTER.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-ssh-host-{tag}-{}-{n}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

/// Run `git args` in `dir` with a hermetic config (no ambient global/system), returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.current_dir(dir)
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_SYSTEM", "/dev/null")
		.args(args)
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout)
		.expect("git stdout utf8")
		.trim()
		.to_owned()
}

/// Whether the local `git` can create a SHA-256 repository (the SHA-256 arms skip otherwise).
fn git_supports_sha256() -> bool {
	let dir = unique_tmp("sha256-probe");
	Command::new("git")
		.args(["init", "--object-format=sha256", dir.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

/// Create a stock-`git` source repository in `object_format` with two commits on `main` and a second
/// branch, returning its canonical absolute path (the path `git-upload-pack '<path>'` serves).
fn make_source(object_format: &str) -> PathBuf {
	let work = unique_tmp(&format!("src-{object_format}"));
	let dir = work.to_str().unwrap();
	assert!(
		Command::new("git")
			.args([
				"init",
				&format!("--object-format={object_format}"),
				"-b",
				"main",
				dir
			])
			.output()
			.expect("git init")
			.status
			.success()
	);
	git(&work, &["config", "user.name", "Src Author"]);
	git(&work, &["config", "user.email", "src@example.com"]);
	std::fs::write(work.join("hello.txt"), b"world\n").unwrap();
	git(&work, &["add", "hello.txt"]);
	git(&work, &["commit", "-m", "first"]);
	git(&work, &["branch", "feature"]);
	std::fs::write(work.join("second.txt"), b"more\n").unwrap();
	git(&work, &["add", "second.txt"]);
	git(&work, &["commit", "-m", "second"]);
	// Canonicalise so macOS `/tmp` → `/private/tmp` matches the path git-upload-pack resolves.
	std::fs::canonicalize(&work).unwrap()
}

/// An SSH URL for `path` through the fake provider (which ignores the host and serves the path).
fn ssh_url(path: &Path) -> String {
	format!("ssh://git@localhost{}", path.display())
}

// --- clone --------------------------------------------------------------------------------------

/// The component clones a stock-git source over SSH into an empty checkout — objects land, `HEAD`
/// resolves to the server tip, the working tree materialises, and the SSH origin is persisted verbatim.
async fn clone_over_ssh<H: HashAlgorithm>() -> Result<()> {
	let source = make_source(H::NAME);
	let url = ssh_url(&source);

	let cli = unique_tmp("clone-dst");
	let work = cli.join("checkout");
	let git_dir = work.join(".git");
	std::fs::create_dir_all(&git_dir)?;

	// `clone` is a static func consuming the two granted descriptors; the host answers ssh-transport.
	let (engine, component) = shared();
	let mut store = store_with_ssh(engine, Box::new(FakeSsh));
	let repo = instantiate_component(engine, &mut store, component).await?;
	let git_desc = grant_dir(&mut store, &git_dir)?;
	let work_desc = grant_dir(&mut store, &work)?;
	repo
		.gitana_repo_porcelain()
		.repository()
		.call_clone(&mut store, git_desc, work_desc, &url)
		.await?
		.map_err(|error| anyhow!("clone: {error:?}"))?;

	// The clone adopted the remote's format and resolved HEAD to its `main` tip, with objects present.
	let client = native_repo::<H>(&git_dir)?;
	let tip = client.refs().resolve_head().await?.expect("HEAD resolves");
	assert_eq!(tip.to_hex(), git(&source, &["rev-parse", "main"]));
	client.commit_tree(tip).await?;
	// The `feature` branch came across too (gitana's clone recreates every remote branch as a local
	// `refs/heads/*`, as the native SSH clone test also asserts).
	let feature = client
		.refs()
		.resolve("refs/heads/feature")
		.await?
		.expect("refs/heads/feature");
	assert_eq!(feature.to_hex(), git(&source, &["rev-parse", "feature"]));

	// The working tree materialised and the SSH origin persisted verbatim (no password to redact).
	assert_eq!(std::fs::read(work.join("hello.txt"))?, b"world\n");
	let config = std::fs::read_to_string(git_dir.join("config"))?;
	assert!(
		config.contains(&format!("url = {url}")),
		"config missing ssh origin url:\n{config}"
	);
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_sha1() {
	clone_over_ssh::<Sha1>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: stock git lacks SHA-256 repository support");
		return;
	}
	clone_over_ssh::<Sha256>().await.unwrap();
}

// --- fetch --------------------------------------------------------------------------------------

/// The component fetches a stock-git source over SSH into an empty client — the tracking ref advances to
/// the server tip and the objects land. Exercises the stateful `multi_ack_detailed` negotiation over SSH.
async fn fetch_over_ssh<H: HashAlgorithm>() -> Result<()> {
	let source = make_source(H::NAME);
	let url = ssh_url(&source);
	let server_main = git(&source, &["rev-parse", "main"]);

	// An empty client repo of the same object format.
	let cli = unique_tmp("fetch-client");
	let client_git = cli.join("client.git");
	std::fs::create_dir_all(&client_git)?;
	native_repo::<H>(&client_git)?.init().await?;

	let mut session = Session::open_with_ssh(&client_git, Box::new(FakeSsh)).await?;
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut session.store, session.handle, &url)
		.await?
		.map_err(|error| anyhow!("fetch: {error:?}"))?;

	assert!(
		outcome
			.updated
			.iter()
			.any(|r| r.name == "refs/remotes/origin/main" && r.id == server_main),
		"expected refs/remotes/origin/main at {server_main}, got {:?}",
		outcome.updated
	);
	assert!(
		outcome.rejected.is_empty(),
		"unexpected rejects: {:?}",
		outcome.rejected
	);

	// The pack landed: the client resolves the tracking ref to the tip and can read its tree.
	let client = native_repo::<H>(&client_git)?;
	let tip = client
		.refs()
		.resolve("refs/remotes/origin/main")
		.await?
		.expect("origin/main");
	assert_eq!(tip.to_hex(), server_main);
	client.commit_tree(tip).await?;
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_sha1() {
	fetch_over_ssh::<Sha1>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: stock git lacks SHA-256 repository support");
		return;
	}
	fetch_over_ssh::<Sha256>().await.unwrap();
}

/// The host single-quotes the remote path (git's `sq_quote`), so a repository path containing shell
/// metacharacters — a space here — round-trips through the remote shell intact. Without the host's
/// quoting, the `FakeSsh`'s `sh -c "git-upload-pack /my repo.git"` would mis-split the path (two args)
/// and the fetch would fail; with it, `'…'` keeps it one argument. This is the injection guard's flip side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_over_ssh_path_with_spaces() {
	// A stock-git source under a directory whose final component contains a space.
	let base = unique_tmp("space");
	let source = base.join("my repo.git");
	std::fs::create_dir_all(&source).unwrap();
	assert!(
		Command::new("git")
			.args(["init", "-b", "main", source.to_str().unwrap()])
			.output()
			.expect("git init")
			.status
			.success()
	);
	git(&source, &["config", "user.name", "Src Author"]);
	git(&source, &["config", "user.email", "src@example.com"]);
	std::fs::write(source.join("hello.txt"), b"world\n").unwrap();
	git(&source, &["add", "hello.txt"]);
	git(&source, &["commit", "-m", "first"]);
	let source = std::fs::canonicalize(&source).unwrap();
	let url = ssh_url(&source);
	let server_main = git(&source, &["rev-parse", "main"]);

	let cli = unique_tmp("space-client");
	let client_git = cli.join("client.git");
	std::fs::create_dir_all(&client_git).unwrap();
	native_repo::<Sha1>(&client_git)
		.unwrap()
		.init()
		.await
		.unwrap();

	let mut session = Session::open_with_ssh(&client_git, Box::new(FakeSsh))
		.await
		.unwrap();
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut session.store, session.handle, &url)
		.await
		.unwrap()
		.expect("fetch of a repo whose path contains a space");
	assert!(
		outcome
			.updated
			.iter()
			.any(|r| r.name == "refs/remotes/origin/main" && r.id == server_main),
		"expected refs/remotes/origin/main at {server_main}, got {:?}",
		outcome.updated
	);
}

// --- push ---------------------------------------------------------------------------------------

/// The component pushes its `HEAD` branch over SSH to an empty stock-git bare server — the branch is
/// created on the server at the client tip, with the packed objects present (read back by stock git).
async fn push_over_ssh<H: HashAlgorithm>() -> Result<()> {
	// An empty bare server repo (no refs yet): git-receive-pack will create `main`.
	let server = unique_tmp(&format!("push-srv-{}", H::NAME));
	assert!(
		Command::new("git")
			.args([
				"init",
				"--bare",
				&format!("--object-format={}", H::NAME),
				"-b",
				"main",
				server.to_str().unwrap(),
			])
			.output()
			.expect("git init --bare")
			.status
			.success()
	);
	let server = std::fs::canonicalize(&server)?;
	let url = ssh_url(&server);

	// A client repo (same format) with one commit on `main`.
	let cli = unique_tmp("push-client");
	let client_git = cli.join("client.git");
	std::fs::create_dir_all(&client_git)?;
	let client = native_repo::<H>(&client_git)?;
	client.init().await?;
	let blob = client.write_blob(b"world\n").await?;
	let tree = client
		.write_tree(&[gitana_repository::TreeBuildEntry {
			path: "hello.txt".to_owned(),
			mode: gitana_repository::FileMode::Regular,
			id: blob,
		}])
		.await?;
	let tip = client
		.commit_on_head(tree, support::AUTHOR, &support::committer(0), "srv\n")
		.await?;

	let mut session = Session::open_with_ssh(&client_git, Box::new(FakeSsh)).await?;
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

	// Stock git on the server now resolves `main` to the client tip (objects transferred, not just the ref).
	assert_eq!(git(&server, &["rev-parse", "main"]), tip.to_hex());
	Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_sha1() {
	push_over_ssh::<Sha1>().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_sha256() {
	if !git_supports_sha256() {
		eprintln!("skipping: stock git lacks SHA-256 repository support");
		return;
	}
	push_over_ssh::<Sha256>().await.unwrap();
}

// --- DOS-drive path rejection -------------------------------------------------------------------

/// A DOS-drive-prefixed URL (a Windows local path such as `C:/repo` / `C:\repo`) is refused by the
/// component before any SSH provider runs. The wasm component cannot know its host's OS, so it
/// conservatively rejects a `<letter>:` path rather than dispatch it to the ssh provider as host `C`
/// (git treats it as a local path on a Windows host). The `FakeSsh` provider is granted but never spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_dos_drive_path_as_local() {
	let cli = unique_tmp("drive-client");
	let client_git = cli.join("client.git");
	std::fs::create_dir_all(&client_git).unwrap();
	native_repo::<Sha1>(&client_git)
		.unwrap()
		.init()
		.await
		.unwrap();

	let mut session = Session::open_with_ssh(&client_git, Box::new(FakeSsh))
		.await
		.unwrap();
	for url in ["C:/repo", "C:\\repo", "c:repo"] {
		let result = session
			.repo
			.gitana_repo_porcelain()
			.repository()
			.call_fetch(&mut session.store, session.handle, url)
			.await
			.unwrap();
		assert!(
			result.is_err(),
			"expected {url:?} to be refused as a local path, got {result:?}"
		);
	}
}

// --- both remote capabilities at once -----------------------------------------------------------

/// Both remote capabilities can be granted to one store via [`store_with`]: with `credentials` AND `ssh`
/// wired together, an SSH fetch still round-trips — proving an embedder can serve authenticated HTTP and
/// SSH remotes from a single session, not one capability or the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_capabilities_can_be_granted_together() {
	let source = make_source("sha1");
	let url = ssh_url(&source);
	let server_main = git(&source, &["rev-parse", "main"]);

	let cli = unique_tmp("both-caps");
	let client_git = cli.join("client.git");
	std::fs::create_dir_all(&client_git).unwrap();
	native_repo::<Sha1>(&client_git)
		.unwrap()
		.init()
		.await
		.unwrap();
	// An (empty) credential store granted alongside the ssh provider — its mere presence proves the two
	// capabilities coexist; the SSH fetch never consults it.
	let cred_store = cli.join("creds");
	std::fs::write(&cred_store, "").unwrap();

	let (engine, component) = shared();
	let mut store = store_with(
		engine,
		Some(Box::new(StoreFileCredentials::new(&cred_store))),
		Some(Box::new(FakeSsh)),
	);
	let repo = instantiate_component(engine, &mut store, component)
		.await
		.unwrap();
	let dir = grant_dir(&mut store, &client_git).unwrap();
	let handle = repo
		.gitana_repo_porcelain()
		.repository()
		.call_open(&mut store, dir)
		.await
		.unwrap()
		.expect("open");
	let outcome = repo
		.gitana_repo_porcelain()
		.repository()
		.call_fetch(&mut store, handle, &url)
		.await
		.unwrap()
		.expect("fetch over ssh with credentials also granted");

	assert!(
		outcome
			.updated
			.iter()
			.any(|r| r.name == "refs/remotes/origin/main" && r.id == server_main),
		"expected refs/remotes/origin/main at {server_main}, got {:?}",
		outcome.updated
	);
}
