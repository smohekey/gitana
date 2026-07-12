//! The pluggable backend behind the host's imported `credentials` capability.

use crate::gitana::repo::credentials::{Credential, CredentialRequest};

/// Answers the guest's `credentials` import — the host side of git's HTTP credential flow. [`State`]
/// holds one of these and forwards the WIT `fill`/`approve`/`reject` calls to it, so an embedder plugs
/// in whatever credential source it trusts (a keychain, a helper, a static store) without touching the
/// wasmtime wiring. The harness ships [`StoreFileCredentials`](crate::StoreFileCredentials) as a
/// working default; a `State` built with no provider answers every `fill` with `None` (anonymous).
///
/// Methods are synchronous: the harness's own sources resolve without awaiting, and an embedder that
/// needs a genuinely async source implements the generated `credentials::Host` on its own store type
/// directly. `fill` returning `None` means "no credential" — the guest then lets the server's `401`
/// stand; `approve`/`reject` are best-effort and report the server's verdict so the source may persist
/// or erase.
///
/// [`State`]: crate::State
pub trait HostCredentialProvider: Send + Sync {
	/// Resolve a credential for `request`, or `None` if the source has none to offer.
	fn fill(&self, request: &CredentialRequest) -> Option<Credential>;

	/// Record that `cred` (for `request`) was accepted by the server, so the source may persist it.
	fn approve(&self, request: &CredentialRequest, cred: &Credential);

	/// Record that `cred` (for `request`) was rejected by the server, so the source may erase it.
	fn reject(&self, request: &CredentialRequest, cred: &Credential);
}
