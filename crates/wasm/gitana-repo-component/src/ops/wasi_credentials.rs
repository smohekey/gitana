//! HTTP credentials backed by the host-granted `credentials` import.
//!
//! Implements [`gitana_remote::CredentialProvider`] over the component's imported credential
//! capability — the wasm counterpart of the CLI's git-backed provider. The component holds no ambient
//! authority to read a netrc, run a helper, or prompt, so [`AuthTransport`](gitana_remote::AuthTransport)
//! drives *this*, and each method forwards to the host over WIT: [`fill`](CredentialProvider::fill) on a
//! `401`, [`approve`](CredentialProvider::approve)/[`reject`](CredentialProvider::reject) to report the
//! outcome.
//!
//! The imports are synchronous component-model calls (no pollable/stream), so each `async fn` here makes
//! the call and returns an already-`Ready` future — the sync-export [`block_on`](crate::block_on) never
//! sees `Pending`. There is no error channel over WIT: a host with nothing to offer returns `none` from
//! `fill`, so conversions map to `Ok(None)`/`Ok(())` and never surface an `Err`.

use anyhow::Result;
use gitana_remote::{Credential, CredentialProvider, CredentialRequest};

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
	async fn fill(&self, request: &CredentialRequest) -> Result<Option<Credential>> {
		Ok(credentials::fill(&to_wit_request(request)).map(from_wit_credential))
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
/// keeps it) and `wwwauth` carried verbatim so a host helper receives the `401` challenge.
fn to_wit_request(request: &CredentialRequest) -> credentials::CredentialRequest {
	credentials::CredentialRequest {
		protocol: request.protocol.clone(),
		host: request.host.clone(),
		path: request.path.clone(),
		username: request.username.clone(),
		wwwauth: request.wwwauth.clone(),
	}
}

/// Native credential → WIT record.
fn to_wit_credential(cred: &Credential) -> credentials::Credential {
	credentials::Credential {
		username: cred.username.clone(),
		password: cred.password.clone(),
	}
}

/// WIT credential → native.
fn from_wit_credential(cred: credentials::Credential) -> Credential {
	Credential {
		username: cred.username,
		password: cred.password,
	}
}
