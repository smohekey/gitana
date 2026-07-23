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
	AuthTransport, Credential, CredentialProvider, CredentialRequest, Filled, Origin,
	ReqwestTransport,
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
	/// A provider reading from `config` (typically `git_config::from_repo`, or `from_ambient`
	/// before a repo exists), resolving a relative askpass helper against `cwd`.
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
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Filled>> {
		let askpass = self.askpass();
		let config = credential_helper::resolve(&self.config, request)?;

		// The credential state threaded through the helper chain — git's single mutable credential. Seeded
		// with the username known before helpers run (the transport's URL-userinfo hint — git's
		// `username_from_proto`, which config never overrides — else the resolved `credential.username`) and
		// the prior multistage round's context. Each helper is fed this and mutates it (adding a
		// username/password, or an `authtype`/`credential`), so a field one helper sets survives to the next.
		// `authtype`/`ephemeral` seed the *carried* context of a multistage round — git retains them across
		// the round (only the secret is cleared), so a continuation helper receives the in-progress scheme and
		// the `ephemeral` marker survives even if the completing helper does not restate it. (`state[]` rides
		// the request, fed to every helper by `get_request_lines`, not the accumulator.)
		//
		// The username a prior round *learned* (`request.carried_username`, git's retained `c->username`) seeds
		// the accumulator too — re-presented to the continuation helper and carried onto the final credential
		// — taking precedence over the resolution hint. It is kept out of `resolve` above (which keys on the
		// stable `request.username`) so a learned username never re-selects the helper chain mid-handshake.
		//
		// The capabilities a prior round negotiated (`request.caps_authtype`/`caps_state`) seed the
		// accumulator. git retains each capability's helper-side bit across a multistage round independently —
		// neither `credential_clear_secrets` nor `credential_fill` resets `capa_authtype`/`capa_state` — so a
		// continuation helper's `authtype`/`credential` (authtype) and `state[]`/`continue` (state) are
		// honoured even when it does not re-advertise the capability. Carried per-capability, not inferred from
		// "a round continued": a round that negotiated only `state` must not enable `authtype`.
		let mut acc = credential_helper::GetOutput {
			username: request
				.carried_username
				.clone()
				.or_else(|| request.username.clone())
				.or(config.username),
			authtype: request.authtype.clone(),
			ephemeral: request.ephemeral,
			caps_authtype: request.caps_authtype,
			caps_state: request.caps_state,
			..credential_helper::GetOutput::default()
		};
		// The helper `get` chain. Matching git's order: adopt the returned state, stop as soon as the
		// credential is complete (a full encoded credential, or both username and password), and only *then*
		// honour a `quit` — a helper that returns a complete credential alongside `quit=1` still succeeds; an
		// incomplete one aborts, as git does.
		let mut chain_reset = false;
		for helper in &config.helpers {
			acc = helper
				.get(request, &acc, config.use_http_path, &self.cwd)
				.await?;
			// A helper's `url=` reset git's whole credential (including the helper list), so record it for
			// the later `approve`/`reject` and stop consulting helpers.
			chain_reset |= acc.reset;
			// Complete once a full encoded credential or both Basic fields are present.
			if acc.has_encoded() || (acc.username.is_some() && acc.password.is_some()) {
				break;
			}
			if acc.quit {
				bail!("credential helper told us to quit");
			}
			if acc.reset {
				// Fall through to prompting for whatever the reset left missing.
				break;
			}
		}
		*self.chain_reset.lock().expect("chain_reset not poisoned") = chain_reset;

		// The multistage signals ride *any* credential (Basic or encoded), honoured only under the
		// mutually-advertised `state` capability — so a helper cannot force a loop it did not opt into, and
		// a stateful helper's `state[]`/`continue` survive even with a username/password credential.
		let (state, more) = if acc.caps_state {
			(acc.state.clone(), acc.more)
		} else {
			(Vec::new(), false)
		};
		// `ephemeral` is under the `authtype` capability and applies to *any* credential — a helper may
		// mark a username/password short-lived too, so it must ride the Basic returns as well as the encoded
		// one (else the transport would cache and pre-emptively reuse a value it was told not to persist).
		let ephemeral = acc.caps_authtype && acc.ephemeral;
		// The capabilities finally negotiated — carried onto the [`Filled`] so the transport re-presents them
		// to the next round independently (a `url=` reset leaves both `false`, ending the multistage).
		let caps_authtype = acc.caps_authtype;
		let caps_state = acc.caps_state;
		// The encoded fields (git's `authtype`/`credential`), honoured only under the mutually-advertised
		// `authtype` capability. In git's flat credential these ride alongside any username/password (git keeps
		// every populated field), so an `authtype` is retained even when this round's credential is Basic.
		let (authtype, credential_value) = if acc.caps_authtype {
			(acc.authtype.clone(), acc.credential.clone())
		} else {
			(None, None)
		};
		let username = acc.username;
		let password = acc.password;
		// A complete credential — a pre-encoded `authtype`+`credential`, or a full Basic pair — is returned
		// without prompting, with any multistage signals.
		let complete_encoded = authtype.is_some() && credential_value.is_some();
		if complete_encoded || (username.is_some() && password.is_some()) {
			return Ok(Some(Filled {
				credential: Credential {
					username,
					password,
					authtype,
					credential: credential_value,
					ephemeral,
				},
				state,
				more,
				caps_authtype,
				caps_state,
			}));
		}
		// About to prompt for a Basic username/password — but only when the challenge actually offers Basic.
		// The transport withholds a Basic credential from a server that did not offer Basic (a Bearer/
		// Negotiate-only challenge, or a `401` with *no* `WWW-Authenticate` at all), so prompting there would
		// resolve a credential guaranteed to be discarded — decline instead and let the `401` stand.
		// Helpers have already run above, so an encoded scheme is still resolved first.
		if !gitana_remote::challenge_offers(&request.wwwauth, "basic") {
			return Ok(None);
		}

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

		// A prompted Basic credential still carries any multistage signals — and the `ephemeral` marker, and
		// any `authtype` a stateful helper left behind (git keeps its own field across the prompt).
		Ok(Some(Filled {
			credential: Credential {
				username: Some(username),
				password: Some(password),
				authtype,
				credential: credential_value,
				ephemeral,
			},
			state,
			more,
			caps_authtype,
			caps_state,
		}))
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
				.store(request, cred, config.use_http_path, &self.cwd)
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
				.erase(request, cred, config.use_http_path, &self.cwd)
				.await;
		}
		Ok(())
	}
}
