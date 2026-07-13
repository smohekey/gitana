//! HTTP credentials backed by the host-granted `credentials` import.
//!
//! Implements [`gitana_remote::CredentialProvider`] over the component's imported credential
//! capability — the wasm counterpart of the CLI's git-backed provider. The component holds no ambient
//! authority to read a netrc, run a helper, or prompt, so [`AuthTransport`](gitana_remote::AuthTransport)
//! drives *this*, and each method forwards to the host over WIT: [`fill`](CredentialProvider::fill) on a
//! `401`, [`approve`](CredentialProvider::approve)/[`reject`](CredentialProvider::reject) to report the
//! outcome. A credential may be Basic or a pre-encoded scheme (Bearer/…), and `fill` carries git's
//! multistage `state`/`more` signals.
//!
//! The imports are synchronous component-model calls (no pollable/stream), so each `async fn` here makes
//! the call and returns an already-`Ready` future — the sync-export [`block_on`](crate::block_on) never
//! sees `Pending`. There is no error channel over WIT: a host with nothing to offer returns `none` from
//! `fill`, so conversions map to `Ok(None)`/`Ok(())` and never surface an `Err`.

use anyhow::Result;
use gitana_remote::{Credential, CredentialProvider, CredentialRequest, Filled};

use crate::bindings::gitana::repo::credentials;

/// A [`CredentialProvider`] that answers by calling the host-imported `credentials` interface.
pub(crate) struct WasiCredentialProvider;

impl WasiCredentialProvider {
	/// A provider over the host credential import. Stateless — the authority is the import itself.
	pub(crate) fn new() -> Self {
		Self
	}
}

impl CredentialProvider for WasiCredentialProvider {
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Filled>> {
		Ok(credentials::fill(&to_wit_request(request)).map(from_wit_filled))
	}

	async fn approve(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		credentials::approve(&to_wit_request(request), &to_wit_credential(cred));
		Ok(())
	}

	async fn reject(&self, request: &CredentialRequest, cred: &Credential) -> Result<()> {
		credentials::reject(&to_wit_request(request), &to_wit_credential(cred));
		Ok(())
	}
}

/// Native request → WIT record. `path` is passed through raw (percent-encoded, as the native side
/// keeps it); `wwwauth` and `state` are carried verbatim so a host helper resumes a multistage round.
fn to_wit_request(request: &CredentialRequest) -> credentials::CredentialRequest {
	credentials::CredentialRequest {
		protocol: request.protocol.clone(),
		host: request.host.clone(),
		path: request.path.clone(),
		username: request.username.clone(),
		carried_username: request.carried_username.clone(),
		wwwauth: request.wwwauth.clone(),
		state: request.state.clone(),
		authtype: request.authtype.clone(),
		ephemeral: request.ephemeral,
		caps_authtype: request.caps_authtype,
		caps_state: request.caps_state,
	}
}

/// Native credential → WIT record (flat, field for field — git's `struct credential`).
fn to_wit_credential(cred: &Credential) -> credentials::Credential {
	credentials::Credential {
		username: cred.username.clone(),
		password: cred.password.clone(),
		authtype: cred.authtype.clone(),
		credential: cred.credential.clone(),
		ephemeral: cred.ephemeral,
	}
}

/// WIT credential → native (flat).
fn from_wit_credential(cred: credentials::Credential) -> Credential {
	Credential {
		username: cred.username,
		password: cred.password,
		authtype: cred.authtype,
		credential: cred.credential,
		ephemeral: cred.ephemeral,
	}
}

/// WIT fill result → native.
fn from_wit_filled(filled: credentials::Filled) -> Filled {
	Filled {
		credential: from_wit_credential(filled.credential),
		state: filled.state,
		more: filled.more,
		caps_authtype: filled.caps_authtype,
		caps_state: filled.caps_state,
	}
}
