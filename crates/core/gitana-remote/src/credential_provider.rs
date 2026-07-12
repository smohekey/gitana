//! The credential-resolution capability.

use std::future::Future;

use anyhow::Result;

use crate::{Credential, CredentialRequest};

/// Resolves and records HTTP credentials the git way. Like [`Identity`]/[`Signer`] in
/// `gitana-porcelain`, the engine *holds* this capability rather than reading netrc, invoking
/// credential helpers, or prompting itself: the CLI adapter implements it over git's credential
/// machinery, a headless caller supplies a no-op, and (in a later slice) the wasm host grants it over
/// WIT. [`AuthTransport`](crate::AuthTransport) drives it — asking for a credential only when the
/// server answers `401`, and reporting the outcome so a helper can persist or erase it.
///
/// Methods are `async` so an implementation can spawn a helper / askpass subprocess without blocking
/// the runtime (`docs/conventions.md`); resolution stays lazy — `fill` is called only on a real
/// challenge.
///
/// [`Identity`]: https://docs.rs/gitana-porcelain
/// [`Signer`]: https://docs.rs/gitana-porcelain
pub trait CredentialProvider {
	/// Resolve a credential for `request`. `Ok(None)` means none is available — anonymous, no
	/// credential configured, no tty to prompt on, or the user declined — and the caller proceeds
	/// unauthenticated, letting the server's `401` stand as the error. `Err` is for a resolution that
	/// genuinely failed (a helper crashed), which aborts the operation.
	fn fill(&self, request: &CredentialRequest) -> impl Future<Output = Result<Option<Credential>>>;

	/// Record that `cred` (for `request`) was accepted by the server (git's `credential approve`) so a
	/// helper may persist it. The `request` carries the protocol/host/path a helper keys its store on —
	/// needed because a credential's location cannot be inferred from the username/password alone (and a
	/// URL-userinfo credential never passed through [`fill`](Self::fill)). Best-effort — a failure here
	/// never fails the operation the credential just authorised.
	fn approve(
		&self,
		request: &CredentialRequest,
		cred: &Credential,
	) -> impl Future<Output = Result<()>>;

	/// Record that `cred` (for `request`) was rejected by the server (git's `credential reject`) so a
	/// helper may erase a now-stale entry. Best-effort, like [`approve`](Self::approve).
	fn reject(
		&self,
		request: &CredentialRequest,
		cred: &Credential,
	) -> impl Future<Output = Result<()>>;
}
