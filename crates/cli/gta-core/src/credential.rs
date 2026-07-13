//! The CLI side of HTTP authentication: resolve credentials the way git does.
//!
//! Implements [`gitana_remote::CredentialProvider`] over the ambient authority the engine
//! deliberately lacks — git config, credential helpers, and interactive prompting. [`AuthTransport`]
//! asks this to `fill` a credential only when a remote answers `401`. Resolution follows git's order:
//! the URL-userinfo username hint (or `credential.username`), then the configured credential-helper
//! chain ([`get`](crate::credential_helper::Helper::get) over `git-credential-*` programs), then an
//! interactive prompt (askpass → terminal) for anything the helpers left missing. `approve`/`reject`
//! run each helper's `store`/`erase` so an accepted credential persists and a rejected one is erased.
//!
//! [`AuthTransport`]: gitana_remote::AuthTransport

use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_config::GitConfig;
use gitana_remote::{
	AuthTransport, Credential, CredentialProvider, CredentialRequest, Origin, ReqwestTransport,
};

use crate::prompt::{self, Echo};
use crate::{credential_helper, http_headers};

/// The CLI's authenticating HTTP transport for `origin`: a native `ReqwestTransport` wrapped with a
/// [`CliCredentialProvider`] reading `config`, seeded with any userinfo the URL carried. Every remote
/// command (`clone`/`fetch`/`push`/`pull`/`trust sync`) builds its transport this way, so git's
/// credential flow applies uniformly. `cwd` is the directory a relative askpass helper resolves
/// against — the worktree root for a repo command, the launch directory for `clone` — matching the
/// directory git runs the helper from. The one transport is threaded through both the advertisement
/// `GET` and the pack `POST`, sharing the credential it resolves. The transport also carries the
/// `http.extraHeader` values `config` sets for `origin.url` (git's URL-matched extra request headers).
pub fn transport_for(
	config: GitConfig,
	origin: &Origin,
	cwd: PathBuf,
) -> Result<AuthTransport<ReqwestTransport, CliCredentialProvider>> {
	let extra_headers = http_headers::extra_headers(&config, &origin.url)?;
	Ok(AuthTransport::with_userinfo(
		ReqwestTransport::with_extra_headers(extra_headers),
		CliCredentialProvider::new(config, cwd),
		origin.url.clone(),
		origin.username.clone(),
		origin.password.clone(),
	))
}

/// Resolves HTTP credentials from git config and interactive prompts. Holds a snapshot of the
/// effective config (the merged system/global/local stack, or the ambient global/system stack for a
/// `clone` that has no local config yet), from which it reads `credential.username` and
/// `core.askPass`, plus the `cwd` a relative askpass helper resolves against.
pub struct CliCredentialProvider {
	config: GitConfig,
	cwd: PathBuf,
	/// Whether the most recent [`fill`](CredentialProvider::fill)'s helper chain was reset by a `url=`
	/// response. git's `credential_from_url` clears the helper list, so the following `approve`/`reject`
	/// must issue no `store`/`erase` — this records that across the separate callback calls (a provider
	/// is per-operation, and `fill` precedes its `approve`/`reject`).
	chain_reset: std::sync::Mutex<bool>,
}

impl CliCredentialProvider {
	/// A provider reading from `config` (typically `git_config::effective_config`, or
	/// `ambient_effective` before a repo exists), resolving a relative askpass helper against `cwd`.
	/// `cwd` is made absolute up front (against the process directory) so joining a relative askpass
	/// path against it, *and* running the helper from it, do not compound into a doubled path.
	pub fn new(config: GitConfig, cwd: PathBuf) -> Self {
		let cwd = if cwd.is_absolute() {
			cwd
		} else {
			std::env::current_dir()
				.map(|base| base.join(&cwd))
				.unwrap_or(cwd)
		};
		Self {
			config,
			cwd,
			chain_reset: std::sync::Mutex::new(false),
		}
	}

	/// The askpass program to prompt through, in git's precedence: `GIT_ASKPASS`, then `core.askPass`,
	/// then `SSH_ASKPASS`. git stops at the first source that is *set* — even to an empty value, which
	/// means "no askpass" (fall back to the terminal) and does **not** continue to a lower-priority
	/// source. So a set-but-empty `GIT_ASKPASS` suppresses the others. A relative helper *path* (one with
	/// a separator) is resolved against [`cwd`](Self::cwd) so it locates the same file git would; a bare
	/// program name is left for `PATH` lookup. `None` means fall back to the controlling terminal.
	fn askpass(&self) -> Option<String> {
		let selected = std::env::var("GIT_ASKPASS")
			.ok()
			.or_else(|| {
				self
					.config
					.get_string("core", None, "askpass")
					.map(str::to_owned)
			})
			.or_else(|| std::env::var("SSH_ASKPASS").ok())
			.filter(|program| !program.is_empty())?;
		// A relative path *with a directory component* resolves against the askpass cwd; a bare program
		// name (a single component) is left for `PATH` lookup, and an absolute path passes through. Using
		// path components (not a hard-coded `/`) recognises a native separator on every platform.
		let path = std::path::Path::new(&selected);
		if path.is_relative() && path.components().count() > 1 {
			Some(self.cwd.join(path).to_string_lossy().into_owned())
		} else {
			Some(selected)
		}
	}
}

impl CredentialProvider for CliCredentialProvider {
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Credential>> {
		let askpass = self.askpass();
		let config = credential_helper::resolve(&self.config, request)?;

		// The username known before helpers run: the transport's hint (URL userinfo — git's
		// `username_from_proto`, which config never overrides) wins, else the resolved `credential.username`.
		let mut username = request.username.clone().or(config.username);
		let mut password = None;

		// The helper `get` chain, feeding forward what is known (and the `401`'s challenge) so a helper can
		// supply the username, the password, or both. `get` returns the *resulting* credential state (it
		// may also reset a field via a `url=` response). Matching git's order: adopt the returned state,
		// stop as soon as both fields are known (success), and only *then* honour a `quit` — a helper that
		// returns a complete credential alongside `quit=1` still succeeds; an incomplete one aborts, as
		// git does (`die("credential helper ... told us to quit")`).
		let mut chain_reset = false;
		for helper in &config.helpers {
			let output = helper
				.get(
					request,
					username.as_deref(),
					password.as_deref(),
					config.use_http_path,
					&self.cwd,
				)
				.await?;
			username = output.username;
			password = output.password;
			// A helper's `url=` reset git's whole credential (including the helper list), so record it for
			// the later `approve`/`reject` and stop consulting helpers — even if the reset credential is
			// itself complete.
			chain_reset |= output.reset;
			if username.is_some() && password.is_some() {
				break;
			}
			if output.quit {
				bail!("credential helper told us to quit");
			}
			if output.reset {
				// Fall through to prompting for whatever the reset left missing.
				break;
			}
		}
		*self.chain_reset.lock().expect("chain_reset not poisoned") = chain_reset;

		// The path git appends to a prompt's URL — only under `useHttpPath`, where the repository path is
		// part of the credential's identity, so a prompt-aware broker keys on the exact URL git shows.
		// git re-encodes the decoded path for display (`credential_format`), so `a%20b` shows as `a%20b`
		// and `a%2Fb` as `a/b`; separators stay literal, a raw byte becomes `%FF`. `request.path` is
		// percent-encoded, so decode to bytes then re-encode the same way as request matching.
		let path_suffix = match (config.use_http_path, request.path.as_deref()) {
			(true, Some(path)) => {
				format!(
					"/{}",
					credential_helper::percent_encode_request_path(&gitana_remote::percent_decode_bytes(
						path
					))
				)
			}
			_ => String::new(),
		};

		// Prompt for whatever the helpers left missing (git's `credential_getpass`): the username first
		// (askpass → terminal), then the password. A `None` from a prompter means there is none available,
		// so decline and let the server's 401 stand.
		let username = match username {
			Some(username) => username,
			None => {
				let prompt = format!(
					"Username for '{}://{}{path_suffix}': ",
					request.protocol, request.host
				);
				match prompt::ask(askpass.as_deref(), &prompt, Echo::Show, &self.cwd).await? {
					// A prompted username is a present value even when empty (git then requests the password,
					// allowing a `:token` credential); `None` means no prompter, so decline.
					Some(username) => username,
					None => return Ok(None),
				}
			}
		};

		let password = match password {
			Some(password) => password,
			None => {
				// git shows the resolved username in the password prompt, **URL-encoded** (a `@` in the
				// username appears as `%40`) — but omits the `username@` entirely when the username is empty
				// (a `:token` credential), so a prompt-aware askpass sees the exact URL git would pass.
				let userinfo = if username.is_empty() {
					String::new()
				} else {
					format!("{}@", gitana_remote::percent_encode_userinfo(&username))
				};
				let prompt = format!(
					"Password for '{}://{userinfo}{}{path_suffix}': ",
					request.protocol, request.host
				);
				match prompt::ask(askpass.as_deref(), &prompt, Echo::Hide, &self.cwd).await? {
					Some(password) => password,
					None => return Ok(None),
				}
			}
		};

		Ok(Some(Credential { username, password }))
	}

	async fn approve(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// git's `credential approve`: hand the accepted credential to every configured helper's `store`.
		// Best-effort — each helper's persistence failure is swallowed inside [`Helper::store`], so this
		// never fails the operation the credential just authorised. But if this credential came from a
		// helper's `url=` reset, git cleared the helper list, so no `store` is issued.
		if *self.chain_reset.lock().expect("chain_reset not poisoned") {
			return Ok(());
		}
		let config = credential_helper::resolve(&self.config, request)?;
		for helper in &config.helpers {
			helper
				.store(
					request,
					&cred.username,
					&cred.password,
					config.use_http_path,
					&self.cwd,
				)
				.await;
		}
		Ok(())
	}

	async fn reject(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// git's `credential reject`: hand the rejected credential to every helper's `erase`. Best-effort.
		// As with `approve`, a `url=` reset during fill cleared the helper list, so no `erase` is issued.
		if *self.chain_reset.lock().expect("chain_reset not poisoned") {
			return Ok(());
		}
		let config = credential_helper::resolve(&self.config, request)?;
		for helper in &config.helpers {
			helper
				.erase(
					request,
					&cred.username,
					&cred.password,
					config.use_http_path,
					&self.cwd,
				)
				.await;
		}
		Ok(())
	}
}
