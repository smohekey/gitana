//! The result of a credential `fill` — the credential plus git's multistage-auth signals.

use crate::Credential;

/// What [`CredentialProvider::fill`](crate::CredentialProvider::fill) resolves for one round of
/// authentication: the [`Credential`] to send, the opaque `state[]` to echo back on the next round
/// (git's `state` capability — empty when the helper is stateless), and whether the helper expects a
/// further round (git's `continue` — `true` only for a non-final stage of a multistage scheme such as
/// NTLM/Kerberos). [`AuthTransport`](crate::AuthTransport) sends the credential and, on a further `401`,
/// re-fills with the new challenge and this `state` when `more` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
	/// The credential to authenticate this round with.
	pub credential: Credential,
	/// The `state[]` values to carry into the next round's
	/// [`CredentialRequest`](crate::CredentialRequest).
	pub state: Vec<String>,
	/// Whether another authentication round is expected (git's `continue`).
	pub more: bool,
	/// Whether the `authtype` capability was negotiated this round (the helper echoed it). git retains a
	/// capability's helper-side bit across a multistage round independently, so this is carried into the
	/// next [`CredentialRequest`](crate::CredentialRequest) — a continuation helper's `authtype`/`credential`
	/// is honoured only if the capability was genuinely negotiated, not merely because a round continued.
	pub caps_authtype: bool,
	/// Whether the `state` capability was negotiated this round — carried like
	/// [`caps_authtype`](Self::caps_authtype). Necessarily `true` whenever [`more`](Self::more) is set (a
	/// continuation is gated on it), but tracked explicitly so the two capabilities stay independent.
	pub caps_state: bool,
}

impl Filled {
	/// A single-round fill of `credential` — no `state`, no further round, no negotiated capabilities (the
	/// common Basic/Bearer case).
	pub fn once(credential: Credential) -> Self {
		Self {
			credential,
			state: Vec::new(),
			more: false,
			caps_authtype: false,
			caps_state: false,
		}
	}
}
