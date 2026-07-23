//! Native wasmtime host harness for the `gitana-repo-component` guest.
//!
//! Instantiates the component with **no preopens**: the only filesystem authority the
//! guest ever receives is the directory descriptor the host mints explicitly
//! ([`grant_dir`]) and passes to `repository.open`.
//!
//! The mirror image of the guest crate's gating: the host is native-only, so on wasm
//! targets this crate compiles to nothing and workspace-wide wasm checks stay green.

#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;
use std::process::Stdio;

use anyhow::Result;
use tokio::process::Child;
use wasmtime::component::{Component, HasSelf, Linker, Resource};
use wasmtime::error::Context as _;
use wasmtime::{Engine, Store};
use wasmtime_wasi::filesystem::{Descriptor, Dir};
use wasmtime_wasi::p2::pipe::{AsyncReadStream, AsyncWriteStream};
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream};
use wasmtime_wasi::{
	DirPerms, FilePerms, OpenMode, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

// wasmtime-wasi constructs its `Dir` from the cap-std major it was built against (3.x),
// while the gitana workspace is on cap-std 4.x — the two must not be conflated, so the
// host names its own compatible cap-std under a distinct import.
use cap_std_host::ambient_authority;

wasmtime::component::bindgen!({
	path: "../gitana-repo-component/wit",
	world: "repo",
	imports: { default: async },
	exports: { default: async },
	with: {
		// `wasi:http` is satisfied by wasmtime-wasi-http's own host bindings (longest-prefix wins over
		// the general `wasi` mapping below), so the guest's outgoing-handler import is host-mediated.
		"wasi:http": wasmtime_wasi_http::p2::bindings::http,
		"wasi": wasmtime_wasi::p2::bindings,
		// Our `ssh-transport.ssh-session` resource is represented by the host's own [`SshSession`]
		// (a running `ssh` child); bindgen still generates the `Host`/`HostSshSession` traits over it.
		"gitana:repo/ssh-transport.ssh-session": SshSession,
	},
});

mod credential_provider;
mod ssh_provider;
mod store_file_credentials;

pub use self::credential_provider::HostCredentialProvider;
pub use self::ssh_provider::HostSshProvider;
pub use self::store_file_credentials::StoreFileCredentials;

/// Store state: the WASI context (no preopens), the `wasi:http` context, the resource
/// table descriptors are minted into, the credential source answering the guest's
/// `credentials` import (`None` = anonymous, every `fill` yields no credential), and the SSH
/// source answering the `ssh-transport` import (`None` = no SSH authority, every `open` fails).
pub struct State {
	ctx: WasiCtx,
	http_ctx: WasiHttpCtx,
	table: ResourceTable,
	credentials: Option<Box<dyn HostCredentialProvider>>,
	ssh: Option<Box<dyn HostSshProvider>>,
}

/// The host representation of the guest's `ssh-transport.ssh-session` resource: a running `ssh`
/// child whose stdin/stdout the guest drives. `stdout`/`stdin` take the child's piped streams (once
/// each) and hand them to the guest as `wasi:io` streams; `finish` awaits the child. The host spawns it
/// with `kill_on_drop`, so a session dropped before `finish` reaps its `ssh`.
pub struct SshSession {
	child: Child,
}

impl WasiView for State {
	fn ctx(&mut self) -> WasiCtxView<'_> {
		WasiCtxView {
			ctx: &mut self.ctx,
			table: &mut self.table,
		}
	}
}

impl WasiHttpView for State {
	fn http(&mut self) -> WasiHttpCtxView<'_> {
		WasiHttpCtxView {
			ctx: &mut self.http_ctx,
			table: &mut self.table,
			hooks: Default::default(),
		}
	}
}

/// A default engine (component-model async support is always on in wasmtime 46).
pub fn engine() -> Result<Engine> {
	Ok(Engine::new(&wasmtime::Config::new())?)
}

/// A store whose WASI context grants **nothing**: no preopens, no args, no env, and no credential
/// source — every guest `fill` yields no credential, so the remote porcelain stays anonymous. (stderr
/// is inherited so a guest panic is visible when a test fails.)
pub fn store(engine: &Engine) -> Store<State> {
	store_with(engine, None, None)
}

/// Like [`store`], but granting `credentials` as the source answering the guest's credential import —
/// so a `401`-gated remote authenticates with what the source resolves. This is the host edge where an
/// embedder plugs in its credential authority (the harness uses [`StoreFileCredentials`]). A convenience
/// over [`store_with`] for the credentials-only case.
pub fn store_with_credentials(
	engine: &Engine,
	credentials: Box<dyn HostCredentialProvider>,
) -> Store<State> {
	store_with(engine, Some(credentials), None)
}

/// Like [`store`], but granting `ssh` as the source answering the guest's `ssh-transport` import — so
/// the remote porcelain can reach an SSH remote by having the host spawn `ssh` on its behalf. This is
/// the host edge where an embedder plugs in its SSH authority (which `ssh`, keys, config); the e2e
/// tests plug in a fake `ssh` that runs `git-upload-pack` / `git-receive-pack` locally. A convenience
/// over [`store_with`] for the ssh-only case.
pub fn store_with_ssh(engine: &Engine, ssh: Box<dyn HostSshProvider>) -> Store<State> {
	store_with(engine, None, Some(ssh))
}

/// Build a store granting any combination of the two remote capabilities: `credentials` (answering the
/// guest's HTTP `401` credential import) and `ssh` (answering its `ssh-transport` import). Either is
/// `None` to leave that capability ungranted. This is the general constructor an embedder reaches for
/// when a component must use both authenticated HTTP *and* SSH remotes in one session; the
/// [`store`]/[`store_with_credentials`]/[`store_with_ssh`] helpers are thin wrappers over it.
pub fn store_with(
	engine: &Engine,
	credentials: Option<Box<dyn HostCredentialProvider>>,
	ssh: Option<Box<dyn HostSshProvider>>,
) -> Store<State> {
	let ctx = WasiCtxBuilder::new().inherit_stderr().build();
	Store::new(
		engine,
		State {
			ctx,
			http_ctx: WasiHttpCtx::new(),
			table: ResourceTable::new(),
			credentials,
			ssh,
		},
	)
}

impl gitana::repo::credentials::Host for State {
	async fn fill(
		&mut self,
		request: gitana::repo::credentials::CredentialRequest,
	) -> Option<gitana::repo::credentials::Filled> {
		self
			.credentials
			.as_ref()
			.and_then(|source| source.fill(&request))
	}

	async fn approve(
		&mut self,
		request: gitana::repo::credentials::CredentialRequest,
		cred: gitana::repo::credentials::Credential,
	) {
		if let Some(source) = &self.credentials {
			source.approve(&request, &cred);
		}
	}

	async fn reject(
		&mut self,
		request: gitana::repo::credentials::CredentialRequest,
		cred: gitana::repo::credentials::Credential,
	) {
		if let Some(source) = &self.credentials {
			source.reject(&request, &cred);
		}
	}
}

impl gitana::repo::ssh_transport::Host for State {
	async fn open(
		&mut self,
		service: String,
		host: String,
		port: Option<u16>,
		user: Option<String>,
		path: String,
	) -> Result<Resource<SshSession>, String> {
		// The component is untrusted authority-wise (the capability model), so its `open` arguments are
		// enforced *here*, at the host boundary, rather than trusting the guest's own parsing — a provider
		// may trust the WIT contract (`git-<service>` only, no option-injection targets).
		guard_ssh_request(&service, &host, user.as_deref(), &path)?;
		// Assemble and shell-escape the remote command in the trusted host boundary — `git-<service>
		// '<path>'`, the path single-quoted (git's `sq_quote`) — so a path containing shell syntax cannot
		// inject into the remote shell a real `ssh` provider runs it through. The provider receives this
		// already-safe command and never handles the raw path.
		let remote_command = format!("{service} {}", sq_quote(&path));
		// The provider builds the ssh *policy* (which command, keys, host resolution); the host owns the
		// transport-critical mechanics and spawns it — piping stdio to bridge into the guest's wasi:io
		// streams, reaping the child if the session is dropped before `finish`, and clearing `GIT_PROTOCOL`
		// so the server stays on protocol v0 (gitana is a v0 client), regardless of the provider's care.
		let mut command = self
			.ssh
			.as_ref()
			.ok_or_else(|| "no ssh transport capability was granted".to_owned())?
			.open(&host, port, user.as_deref(), &remote_command)?;
		command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.kill_on_drop(true)
			.env_remove("GIT_PROTOCOL");
		let child = command.spawn().map_err(|e| format!("spawning ssh: {e}"))?;
		self
			.table
			.push(SshSession { child })
			.map_err(|e| format!("ssh-session resource table full: {e}"))
	}
}

/// Enforce the `ssh-transport` capability's scope on a guest `open` request, before any provider runs.
/// The component holds no ambient authority, so a granted SSH capability must mean exactly "run
/// `git-upload-pack` / `git-receive-pack` against a remote" — not "spawn an arbitrary process" or "pass
/// an option-injecting target to `ssh`". The guest's own `RemoteUrl` parsing runs the same guards, but
/// the host cannot trust a possibly-malicious component to have done so:
///
/// - `service` is allow-listed to the two git pack services (else a guest could name any binary a naive
///   provider would `Command::new`);
/// - a `-`-leading `host`/`user`/`path` is refused — git's CVE-2017-1000117 guard, since the ssh
///   destination argument is `[user@]host` and a leading `-` on it (or on the remote path) would reach
///   `ssh` / the remote `git-upload-pack` as an option (e.g. `-oProxyCommand=…`);
/// - an empty host is refused (a malformed request, and `@`-prefixed with a user it is nonsensical).
fn guard_ssh_request(
	service: &str,
	host: &str,
	user: Option<&str>,
	path: &str,
) -> Result<(), String> {
	if !matches!(service, "git-upload-pack" | "git-receive-pack") {
		return Err(format!(
			"ssh-transport service must be git-upload-pack or git-receive-pack, not {service:?}"
		));
	}
	if host.is_empty() {
		return Err("ssh-transport host is empty".to_owned());
	}
	// Reject a `-`-leading host, user, or path *independently*. The host cannot assume how a given
	// provider assembles the `ssh` command line: git's own tokenisation makes a `-`-leading host safe
	// only when it is glued behind `user@` in a single argument, but a provider that passes the host as
	// its own argument would hand `-oProxyCommand=…` straight to `ssh` as an option. So — unlike the
	// native `reject_option_injection`, which knows gitana builds `user@host` itself — this boundary
	// refuses every component that could reach `ssh` (or the remote git service) as an option, even a
	// `-`-leading host behind a user (git allows `git@-h`; a provider-agnostic capability cannot).
	if host.starts_with('-') {
		return Err(format!(
			"strange ssh host {host:?} blocked (looks like a command-line option)"
		));
	}
	if let Some(user) = user
		&& user.starts_with('-')
	{
		return Err(format!(
			"strange ssh user {user:?} blocked (looks like a command-line option)"
		));
	}
	if path.starts_with('-') {
		return Err(format!(
			"strange ssh path {path:?} blocked (looks like a command-line option)"
		));
	}
	Ok(())
}

/// POSIX single-quote `s` for the remote shell — wrap in `'…'`, rendering an embedded `'` as `'\''`
/// (git's `sq_quote`), so a repository path with shell metacharacters reaches the remote
/// `git-upload-pack` / `git-receive-pack` intact rather than being interpreted (or injecting). The host
/// applies this before handing the remote command to a provider, keeping the escaping in the trusted
/// boundary. Mirrors `gitana_remote`'s native `sq_quote`.
fn sq_quote(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('\'');
	for ch in s.chars() {
		if ch == '\'' {
			out.push_str("'\\''");
		} else {
			out.push(ch);
		}
	}
	out.push('\'');
	out
}

impl gitana::repo::ssh_transport::HostSshSession for State {
	async fn stdout(&mut self, self_: Resource<SshSession>) -> Resource<DynInputStream> {
		// Take the child's stdout (once) and bridge it into a `wasi:io` input-stream the guest reads the
		// advertisement, ACKs, and packfile from.
		let stdout = self
			.table
			.get_mut(&self_)
			.expect("ssh-session resource is live")
			.child
			.stdout
			.take()
			.expect("ssh-session stdout is taken at most once");
		let stream: DynInputStream = Box::new(AsyncReadStream::new(stdout));
		self
			.table
			.push(stream)
			.expect("input-stream resource table push")
	}

	async fn stdin(&mut self, self_: Resource<SshSession>) -> Resource<DynOutputStream> {
		// Take the child's stdin (once) and bridge it into a `wasi:io` output-stream the guest writes its
		// request to; dropping that stream closes stdin, signalling end-of-request to the server. The 8 KiB
		// write budget matches wasmtime's own stdio streams.
		let stdin = self
			.table
			.get_mut(&self_)
			.expect("ssh-session resource is live")
			.child
			.stdin
			.take()
			.expect("ssh-session stdin is taken at most once");
		let stream: DynOutputStream = Box::new(AsyncWriteStream::new(8192, stdin));
		self
			.table
			.push(stream)
			.expect("output-stream resource table push")
	}

	async fn finish(&mut self, self_: Resource<SshSession>) -> Result<(), String> {
		// Await the child: a nonzero exit is a transport error even if a complete pack was already read,
		// matching stock git (a wrapper may produce a parseable pack and then fail).
		let status = self
			.table
			.get_mut(&self_)
			.map_err(|e| format!("ssh-session resource lookup failed: {e}"))?
			.child
			.wait()
			.await
			.map_err(|e| format!("waiting for ssh to exit: {e}"))?;
		if status.success() {
			Ok(())
		} else {
			Err(format!("ssh exited with {status}"))
		}
	}

	async fn drop(&mut self, rep: Resource<SshSession>) -> wasmtime::Result<()> {
		// Dropping the `SshSession` drops the child; the provider set `kill_on_drop`, so a session dropped
		// before `finish` (e.g. the advertisement read failed) reaps its `ssh` rather than leaking it.
		self.table.delete(rep)?;
		Ok(())
	}
}

/// Instantiate the component at `component_path` against the p2 WASI linker.
pub async fn instantiate(
	engine: &Engine,
	store: &mut Store<State>,
	component_path: &Path,
) -> Result<Repo> {
	let component = Component::from_file(engine, component_path)
		.with_context(|| format!("loading component {}", component_path.display()))?;
	let mut linker = Linker::new(engine);
	wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
	wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
	// Our own `credentials` import: unlike `wasi:http`'s prebuilt linker, its host side is wired here.
	gitana::repo::credentials::add_to_linker::<_, HasSelf<_>>(&mut linker, |state: &mut State| {
		state
	})?;
	// Our own `ssh-transport` import: the host spawns `ssh` and bridges its stdio (like `credentials`,
	// its host side is wired here rather than by a prebuilt wasi linker).
	gitana::repo::ssh_transport::add_to_linker::<_, HasSelf<_>>(&mut linker, |state: &mut State| {
		state
	})?;
	Ok(Repo::instantiate_async(store, &component, &linker).await?)
}

/// Mint a `wasi:filesystem` directory descriptor for `host_path` and push it into the
/// store's resource table — the capability subsequently handed to `repository.open`.
/// This is the host-side edge where ambient authority is exercised, on the host's
/// behalf, exactly once per granted directory.
pub fn grant_dir(store: &mut Store<State>, host_path: &Path) -> Result<Resource<Descriptor>> {
	let dir = cap_std_host::fs::Dir::open_ambient_dir(host_path, ambient_authority())
		.with_context(|| format!("opening {}", host_path.display()))?;
	let dir = Dir::new(
		dir,
		DirPerms::all(),
		FilePerms::all(),
		OpenMode::READ | OpenMode::WRITE,
		false,
	);
	Ok(store.data_mut().table.push(Descriptor::Dir(dir))?)
}

#[cfg(test)]
mod tests {
	use super::{guard_ssh_request, sq_quote};

	#[test]
	fn single_quotes_paths_git_style() {
		assert_eq!(sq_quote("/srv/repo.git"), "'/srv/repo.git'");
		// A space stays inside the quotes (one remote-shell argument, not two).
		assert_eq!(sq_quote("/srv/my repo.git"), "'/srv/my repo.git'");
		// An embedded single quote is closed, escaped, and reopened, so it cannot break out of the quoting.
		assert_eq!(sq_quote("a'b"), "'a'\\''b'");
		// Shell metacharacters are neutralised (they sit literally inside the quotes).
		assert_eq!(sq_quote("/r; rm -rf /"), "'/r; rm -rf /'");
	}

	#[test]
	fn accepts_a_well_formed_git_service_request() {
		assert!(
			guard_ssh_request(
				"git-upload-pack",
				"example.com",
				Some("git"),
				"/srv/repo.git"
			)
			.is_ok()
		);
		assert!(guard_ssh_request("git-receive-pack", "example.com", None, "repo.git").is_ok());
	}

	#[test]
	fn rejects_a_non_git_service() {
		// The capability is scoped to the two git pack services — a guest cannot name another binary a
		// naive provider would `Command::new`.
		assert!(guard_ssh_request("/bin/sh", "example.com", None, "/repo.git").is_err());
		assert!(guard_ssh_request("git-upload-archive", "example.com", None, "/repo.git").is_err());
		assert!(guard_ssh_request("", "example.com", None, "/repo.git").is_err());
	}

	#[test]
	fn rejects_option_injection_targets() {
		// A `-`-leading bare host, user, or path would reach `ssh` / the remote git service as an option
		// (CVE-2017-1000117 class) — refused at the host boundary regardless of a provider's own care.
		assert!(
			guard_ssh_request(
				"git-upload-pack",
				"-oProxyCommand=payload",
				None,
				"/repo.git"
			)
			.is_err()
		);
		assert!(
			guard_ssh_request(
				"git-upload-pack",
				"example.com",
				Some("-oProxyCommand=x"),
				"/repo.git"
			)
			.is_err()
		);
		assert!(guard_ssh_request("git-upload-pack", "example.com", None, "-oProxyCommand=x").is_err());
		// A `-`-leading host is refused *even behind a user*: git allows `git@-h` (it glues `user@host`
		// into one ssh argument), but this boundary cannot assume a provider does — so `git@-oProxy…`
		// with an option-like host is blocked here, stricter than native git on purpose.
		assert!(
			guard_ssh_request(
				"git-upload-pack",
				"-oProxyCommand=x",
				Some("git"),
				"/repo.git"
			)
			.is_err()
		);
		// An empty host is malformed and refused.
		assert!(guard_ssh_request("git-upload-pack", "", None, "/repo.git").is_err());
	}
}
