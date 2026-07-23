//! [`HelperChainProvider`] — git's credential-helper chain as a headless [`CredentialProvider`].
//!
//! This runs the part of git's credential flow that has an ambient answer: resolve which helpers a
//! request configures ([`resolve`](crate::resolve::resolve)), then drive each helper's `get` until the
//! credential is complete. It never prompts — a gap the helpers leave is returned as "no credential"
//! (`fill` → `None`), so the caller stays anonymous and the server's `401` stands. `approve`/`reject`
//! delegate to the helpers' `store`/`erase`, git-faithfully.
//!
//! Two consumers wrap it. The `gta` CLI adds an interactive prompt fallback on the gap (calling
//! [`run_chain`](HelperChainProvider::run_chain) and prompting for whatever [`ChainOutcome`] left
//! missing). Code Henge wraps it read-only — using its headless `fill` as-is and turning `approve`/
//! `reject` into no-ops so a background reconcile never mutates the user's keychain.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use gitana_config::GitConfig;
use gitana_remote::{Credential, CredentialProvider, CredentialRequest, Filled};

use crate::helper::GetOutput;
use crate::resolve;

/// Resolves HTTP credentials from git config and the configured credential-helper chain — the headless
/// core of git's credential flow, with no interactive prompt. Holds a snapshot of the effective config
/// (the merged system/global/local stack, or the ambient global/system stack for a `clone` that has no
/// local config yet), from which it reads `credential.username`, `credential.useHttpPath`, and the
/// `credential.helper` chain, plus the `cwd` a relative or `!`-shell helper resolves against.
pub struct HelperChainProvider {
	config: GitConfig,
	cwd: PathBuf,
	/// Whether the most recent [`run_chain`](Self::run_chain)'s helper chain was reset by a `url=`
	/// response. git's `credential_from_url` clears the helper list, so the following `approve`/`reject`
	/// must issue no `store`/`erase` — this records that across the separate callback calls (a provider
	/// is per-operation, and `run_chain`/`fill` precedes its `approve`/`reject`).
	chain_reset: Mutex<bool>,
}

impl HelperChainProvider {
	/// A provider reading from `config`, resolving a relative helper against `cwd`. `cwd` is made
	/// absolute up front (against the process directory) so joining a relative helper path against it,
	/// *and* running the helper from it, do not compound into a doubled path.
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
			chain_reset: Mutex::new(false),
		}
	}

	/// A provider over git's ambient config stack — system + global, no repository — for resolving a
	/// `clone`'s credentials before a checkout exists. Relative helpers resolve against the command's
	/// working directory (`-C`, else the process cwd), matching git.
	pub async fn from_ambient() -> Result<Self> {
		Ok(Self::new(
			gitana_config_native::from_ambient().await?,
			cwd(),
		))
	}

	/// A provider over the repository's merged config stack — system + global + local — for resolving a
	/// `fetch`'s credentials. `git_dir` is the repository's git directory and `common` its shared config
	/// directory (the same pair [`gitana_config_native::from_repo`] takes).
	pub async fn from_repo(git_dir: &Path, common: &Path) -> Result<Self> {
		Ok(Self::new(
			gitana_config_native::from_repo(git_dir, common).await?,
			cwd(),
		))
	}

	/// The effective config this provider reads (for a wrapper that also needs it, e.g. `gta`'s askpass
	/// selection).
	pub fn config(&self) -> &GitConfig {
		&self.config
	}

	/// The directory a relative or `!`-shell helper resolves and runs against (for a wrapper that prompts
	/// through a relative askpass helper resolved the same way).
	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	/// Run git's credential-helper chain for `request` and return the resolved [`ChainOutcome`], **without
	/// prompting**. Resolution follows git's order: the URL-userinfo username hint (or `credential.username`)
	/// seeds the credential, then each configured helper's `get` mutates it in turn until the credential is
	/// complete, a helper says `quit`, or a `url=` reset clears the chain. Records whether the chain was
	/// reset for the following [`approve`](Self::approve)/[`reject`](Self::reject).
	pub async fn run_chain(&self, request: &CredentialRequest) -> Result<ChainOutcome> {
		let config = resolve::resolve(&self.config, request)?;

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
		let mut acc = GetOutput {
			username: request
				.carried_username
				.clone()
				.or_else(|| request.username.clone())
				.or(config.username),
			authtype: request.authtype.clone(),
			ephemeral: request.ephemeral,
			caps_authtype: request.caps_authtype,
			caps_state: request.caps_state,
			..GetOutput::default()
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
				// Stop the chain; the caller prompts (or declines) for whatever the reset left missing.
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
		let (authtype, credential) = if acc.caps_authtype {
			(acc.authtype.clone(), acc.credential.clone())
		} else {
			(None, None)
		};
		Ok(ChainOutcome {
			username: acc.username,
			password: acc.password,
			authtype,
			credential,
			ephemeral,
			state,
			more,
			caps_authtype,
			caps_state,
			use_http_path: config.use_http_path,
		})
	}
}

impl CredentialProvider for HelperChainProvider {
	/// The headless resolution: run the helper chain and hand back a complete credential, or `None` when
	/// the chain left a gap — no prompt, so the caller stays anonymous and the `401` stands. A wrapper
	/// wanting an interactive fallback calls [`run_chain`](Self::run_chain) instead and prompts on the gap.
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Filled>> {
		Ok(self.run_chain(request).await?.into_filled())
	}

	async fn approve(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// git's `credential approve`: hand the accepted credential to every configured helper's `store`.
		// Best-effort — each helper's persistence failure is swallowed inside `Helper::store`, so this
		// never fails the operation the credential just authorised. But if this credential came from a
		// helper's `url=` reset, git cleared the helper list, so no `store` is issued.
		if *self.chain_reset.lock().expect("chain_reset not poisoned") {
			return Ok(());
		}
		let config = resolve::resolve(&self.config, request)?;
		for helper in &config.helpers {
			helper
				.store(request, cred, config.use_http_path, &self.cwd)
				.await;
		}
		Ok(())
	}

	async fn reject(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		// git's `credential reject`: hand the rejected credential to every helper's `erase`. Best-effort.
		// As with `approve`, a `url=` reset during the chain cleared the helper list, so no `erase` is issued.
		if *self.chain_reset.lock().expect("chain_reset not poisoned") {
			return Ok(());
		}
		let config = resolve::resolve(&self.config, request)?;
		for helper in &config.helpers {
			helper
				.erase(request, cred, config.use_http_path, &self.cwd)
				.await;
		}
		Ok(())
	}
}

/// The credential the helper chain resolved for one authentication round, before any interactive
/// prompt — git's flat credential after the `get` chain, with its multistage signals and negotiated
/// capabilities. A caller either turns it straight into a [`Filled`] ([`into_filled`](Self::into_filled),
/// what the headless [`fill`](HelperChainProvider::fill) does) or, when it is incomplete, prompts for the
/// missing field and rebuilds the credential carrying these same signals.
///
/// Every field is already capability-gated: `authtype`/`credential`/`ephemeral` are present only when the
/// `authtype` capability was mutually advertised, and `state`/`more` only under the `state` capability —
/// so a consumer never re-derives git's gating rules.
pub struct ChainOutcome {
	/// The resolved username (a URL/config hint or one a helper supplied), if any.
	pub username: Option<String>,
	/// The resolved password/secret, if a helper supplied one.
	pub password: Option<String>,
	/// A pre-encoded scheme (git's `authtype`, e.g. Bearer/Digest) under the `authtype` capability.
	pub authtype: Option<String>,
	/// The pre-encoded credential value paired with [`authtype`](Self::authtype) (git's `credential`).
	pub credential: Option<String>,
	/// The credential is short-lived (git's `ephemeral`) — do not persist or pre-emptively reuse it.
	pub ephemeral: bool,
	/// Opaque `state[]` to echo back on the next multistage round.
	pub state: Vec<String>,
	/// A further multistage round is expected (git's `continue`).
	pub more: bool,
	/// The `authtype` capability was mutually advertised — re-present it next round.
	pub caps_authtype: bool,
	/// The `state` capability was mutually advertised — re-present it next round.
	pub caps_state: bool,
	/// The resolved `credential.useHttpPath` for this request — whether the repository path is part of
	/// the credential's identity. Not a credential field; carried so a caller that prompts on the gap can
	/// reproduce git's prompt URL (which appends the path only under `useHttpPath`).
	pub use_http_path: bool,
}

impl ChainOutcome {
	/// Whether the chain resolved a complete credential — a pre-encoded `authtype`+`credential`, or a full
	/// Basic username/password pair — that needs no prompt. An incomplete outcome leaves a gap for the
	/// caller to prompt for (or decline).
	pub fn is_complete(&self) -> bool {
		(self.authtype.is_some() && self.credential.is_some())
			|| (self.username.is_some() && self.password.is_some())
	}

	/// The complete credential as a [`Filled`] carrying its multistage signals, or `None` when the chain
	/// left a gap. This is the headless resolution: a `None` here means "stay anonymous", not "prompt".
	pub fn into_filled(self) -> Option<Filled> {
		if !self.is_complete() {
			return None;
		}
		Some(Filled {
			credential: Credential {
				username: self.username,
				password: self.password,
				authtype: self.authtype,
				credential: self.credential,
				ephemeral: self.ephemeral,
			},
			state: self.state,
			more: self.more,
			caps_authtype: self.caps_authtype,
			caps_state: self.caps_state,
		})
	}
}

/// The directory a relative or `!`-shell helper resolves and runs against for an ambient
/// ([`from_ambient`](HelperChainProvider::from_ambient))/repo
/// ([`from_repo`](HelperChainProvider::from_repo)) provider: the command's working directory (`-C`, set
/// by the CLI edge), else the process cwd — matching the directory git runs a helper from. A background
/// service that never sets the `-C` task-local gets the process cwd.
fn cwd() -> PathBuf {
	gitana_config_native::command_cwd()
		.or_else(|| std::env::current_dir().ok())
		.unwrap_or_else(|| PathBuf::from("."))
}
