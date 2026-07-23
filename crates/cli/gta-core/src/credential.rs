//! The CLI side of HTTP authentication: resolve credentials the way git does.
//!
//! Implements [`gitana_remote::CredentialProvider`] over the ambient authority the engine
//! deliberately lacks — git config, credential helpers, and interactive prompting. The
//! credential-helper chain itself lives in the shared [`gitana_credential`] crate (so Code Henge
//! resolves credentials the same way); [`CliCredentialProvider`] wraps it and adds the one thing a CLI
//! has that a headless service does not — an **interactive prompt** for whatever the helpers left
//! missing. [`AuthTransport`] asks this to `fill` a credential only when a remote answers `401`.
//! Resolution follows git's order: the URL-userinfo username hint (or `credential.username`), then the
//! configured credential-helper chain, then an interactive prompt (askpass → terminal) for anything the
//! helpers left missing. `approve`/`reject` delegate to the shared provider's helper `store`/`erase` so
//! an accepted credential persists and a rejected one is erased.
//!
//! [`AuthTransport`]: gitana_remote::AuthTransport

use std::path::PathBuf;

use anyhow::Result;
use gitana_config::GitConfig;
use gitana_credential::HelperChainProvider;
use gitana_remote::{
	AuthTransport, Credential, CredentialProvider, CredentialRequest, Filled, Origin,
	ReqwestTransport,
};

use crate::http_headers;
use crate::prompt::{self, Echo};

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

/// Resolves HTTP credentials from git config, the credential-helper chain, and interactive prompts. The
/// chain (and its `approve`/`reject`) is the shared headless [`HelperChainProvider`]; this adds the
/// prompt fallback a CLI needs. The provider holds the effective config and the `cwd` a relative askpass
/// helper resolves against.
pub struct CliCredentialProvider {
	inner: HelperChainProvider,
}

impl CliCredentialProvider {
	/// A provider reading from `config` (typically `git_config::from_repo`, or `from_ambient`
	/// before a repo exists), resolving a relative askpass helper against `cwd`. The shared
	/// [`HelperChainProvider`] makes `cwd` absolute up front so a relative askpass path *and* running a
	/// helper from it do not compound into a doubled path.
	pub fn new(config: GitConfig, cwd: PathBuf) -> Self {
		Self {
			inner: HelperChainProvider::new(config, cwd),
		}
	}

	/// The askpass program to prompt through, in git's precedence: `GIT_ASKPASS`, then `core.askPass`,
	/// then `SSH_ASKPASS`. git stops at the first source that is *set* — even to an empty value, which
	/// means "no askpass" (fall back to the terminal) and does **not** continue to a lower-priority
	/// source. So a set-but-empty `GIT_ASKPASS` suppresses the others. A relative helper *path* (one with
	/// a separator) is resolved against the provider's `cwd` so it locates the same file git would; a bare
	/// program name is left for `PATH` lookup. `None` means fall back to the controlling terminal.
	fn askpass(&self) -> Option<String> {
		let selected = std::env::var("GIT_ASKPASS")
			.ok()
			.or_else(|| {
				self
					.inner
					.config()
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
			Some(self.inner.cwd().join(path).to_string_lossy().into_owned())
		} else {
			Some(selected)
		}
	}
}

impl CredentialProvider for CliCredentialProvider {
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Filled>> {
		// Run git's headless helper chain first. A complete credential is returned without prompting,
		// carrying its multistage signals — exactly the headless provider's own `fill`.
		let outcome = self.inner.run_chain(request).await?;
		if outcome.is_complete() {
			return Ok(outcome.into_filled());
		}

		// The chain left a gap. About to prompt for a Basic username/password — but only when the challenge
		// actually offers Basic. The transport withholds a Basic credential from a server that did not offer
		// Basic (a Bearer/Negotiate-only challenge, or a `401` with *no* `WWW-Authenticate` at all), so
		// prompting there would resolve a credential guaranteed to be discarded — decline instead and let the
		// `401` stand. Helpers have already run above, so an encoded scheme is still resolved first.
		if !gitana_remote::challenge_offers(&request.wwwauth, "basic") {
			return Ok(None);
		}

		let askpass = self.askpass();

		// The path git appends to a prompt's URL — only under `useHttpPath`, where the repository path is
		// part of the credential's identity, so a prompt-aware broker keys on the exact URL git shows.
		// git re-encodes the decoded path for display (`credential_format`), so `a%20b` shows as `a%20b`
		// and `a%2Fb` as `a/b`; separators stay literal, a raw byte becomes `%FF`. `request.path` is
		// percent-encoded, so decode to bytes then re-encode the same way as request matching.
		let path_suffix = match (outcome.use_http_path, request.path.as_deref()) {
			(true, Some(path)) => {
				format!(
					"/{}",
					gitana_credential::percent_encode_request_path(&gitana_remote::percent_decode_bytes(
						path
					))
				)
			}
			_ => String::new(),
		};

		// Prompt for whatever the helpers left missing (git's `credential_getpass`): the username first
		// (askpass → terminal), then the password. A `None` from a prompter means there is none available,
		// so decline and let the server's 401 stand.
		let username = match outcome.username.clone() {
			Some(username) => username,
			None => {
				let prompt = format!(
					"Username for '{}://{}{path_suffix}': ",
					request.protocol, request.host
				);
				match prompt::ask(askpass.as_deref(), &prompt, Echo::Show, self.inner.cwd()).await? {
					// A prompted username is a present value even when empty (git then requests the password,
					// allowing a `:token` credential); `None` means no prompter, so decline.
					Some(username) => username,
					None => return Ok(None),
				}
			}
		};

		let password = match outcome.password.clone() {
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
				match prompt::ask(askpass.as_deref(), &prompt, Echo::Hide, self.inner.cwd()).await? {
					Some(password) => password,
					None => return Ok(None),
				}
			}
		};

		// A prompted Basic credential still carries any multistage signals — and the `ephemeral` marker, and
		// any `authtype` a stateful helper left behind (git keeps its own field across the prompt).
		Ok(Some(Filled {
			credential: Credential {
				username: Some(username),
				password: Some(password),
				authtype: outcome.authtype,
				credential: outcome.credential,
				ephemeral: outcome.ephemeral,
			},
			state: outcome.state,
			more: outcome.more,
			caps_authtype: outcome.caps_authtype,
			caps_state: outcome.caps_state,
		}))
	}

	async fn approve(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// Delegate to the shared provider's git-faithful `credential approve` (each helper's `store`,
		// best-effort, suppressed after a `url=` reset).
		self.inner.approve(request, cred).await
	}

	async fn reject(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// Delegate to the shared provider's git-faithful `credential reject` (each helper's `erase`).
		self.inner.reject(request, cred).await
	}
}
