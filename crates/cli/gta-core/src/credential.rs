//! The CLI side of HTTP authentication: resolve credentials the way git does.
//!
//! Implements [`gitana_remote::CredentialProvider`] over the ambient authority the engine
//! deliberately lacks — git config and interactive prompting. [`AuthTransport`] asks this to `fill` a
//! credential only when a remote answers `401`; slice 1 resolves a username from the URL userinfo (a
//! hint the transport passes in) or `credential.username`, then prompts (askpass → terminal) for
//! anything still missing. Credential *helpers* and persistence (`approve`/`reject`) arrive in a later
//! slice, so those are no-ops here.
//!
//! [`AuthTransport`]: gitana_remote::AuthTransport

use std::path::PathBuf;

use anyhow::Result;
use gitana_config::GitConfig;
use gitana_remote::{
	AuthTransport, Credential, CredentialProvider, CredentialRequest, Origin, ReqwestTransport,
};

use crate::prompt::{self, Echo};

/// The CLI's authenticating HTTP transport for `origin`: a native `ReqwestTransport` wrapped with a
/// [`CliCredentialProvider`] reading `config`, seeded with any userinfo the URL carried. Every remote
/// command (`clone`/`fetch`/`push`/`pull`/`trust sync`) builds its transport this way, so git's
/// credential flow applies uniformly. `cwd` is the directory a relative askpass helper resolves
/// against — the worktree root for a repo command, the launch directory for `clone` — matching the
/// directory git runs the helper from. The one transport is threaded through both the advertisement
/// `GET` and the pack `POST`, sharing the credential it resolves.
pub fn transport_for(
	config: GitConfig,
	origin: &Origin,
	cwd: PathBuf,
) -> AuthTransport<ReqwestTransport, CliCredentialProvider> {
	AuthTransport::with_userinfo(
		ReqwestTransport::new(),
		CliCredentialProvider::new(config, cwd),
		origin.url.clone(),
		origin.username.clone(),
		origin.password.clone(),
	)
}

/// Resolves HTTP credentials from git config and interactive prompts. Holds a snapshot of the
/// effective config (the merged system/global/local stack, or the ambient global/system stack for a
/// `clone` that has no local config yet), from which it reads `credential.username` and
/// `core.askPass`, plus the `cwd` a relative askpass helper resolves against.
pub struct CliCredentialProvider {
	config: GitConfig,
	cwd: PathBuf,
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
		Self { config, cwd }
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
		// The bare `protocol://host` git uses in its prompts (the path is only shown under useHttpPath,
		// a later slice).
		let base = format!("{}://{}", request.protocol, request.host);

		// Username: the transport's hint (URL userinfo) wins, then `credential.username`, then a prompt.
		let username = match request.username.clone().or_else(|| {
			self
				.config
				.get_string("credential", None, "username")
				.map(str::to_owned)
		}) {
			Some(username) => username,
			None => {
				let prompt = format!("Username for '{base}': ");
				match prompt::ask(askpass.as_deref(), &prompt, Echo::Show, &self.cwd).await? {
					// A prompted username is a present value even when empty (git then requests the password,
					// allowing a `:token` credential); `None` means no prompter, so decline and let the 401 stand.
					Some(username) => username,
					None => return Ok(None),
				}
			}
		};

		// Password/token: always prompted in slice 1 (helpers, which could supply it non-interactively,
		// come later). git shows the resolved username in the password prompt, **URL-encoded** (a `@` in
		// the username appears as `%40`) — but omits the `username@` entirely when the username is empty
		// (a `:token` credential), so a prompt-aware askpass sees the exact URL git would pass.
		let userinfo = if username.is_empty() {
			String::new()
		} else {
			format!("{}@", gitana_remote::percent_encode_userinfo(&username))
		};
		let prompt = format!(
			"Password for '{}://{userinfo}{}': ",
			request.protocol, request.host
		);
		let Some(password) = prompt::ask(askpass.as_deref(), &prompt, Echo::Hide, &self.cwd).await?
		else {
			return Ok(None);
		};

		Ok(Some(Credential { username, password }))
	}

	async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
		// Persistence lands with credential helpers (a later slice); nothing to record yet.
		Ok(())
	}

	async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
		Ok(())
	}
}
