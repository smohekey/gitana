//! A credential lookup key, derived from the remote URL.

/// The context a [`CredentialProvider`](crate::CredentialProvider) resolves a credential for — git's
/// credential attributes. `protocol` and `host` always identify the server; `path` is the repository
/// path (git passes it only when `credential.useHttpPath` is set, so the provider decides whether to
/// key on it); `username` is a known username the credential must match (from URL userinfo or
/// `credential.username`), narrowing the lookup and pre-filling a helper/prompt; `wwwauth` carries the
/// `WWW-Authenticate` challenge(s) from the `401`, forwarded to a helper as `wwwauth[]` lines (git
/// populates these only when filling, and clears them once a credential is resolved — so they ride a
/// [`fill`](crate::CredentialProvider::fill) request but not an approve/reject).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
	/// The URL scheme, e.g. `https`.
	pub protocol: String,
	/// The authority, `host` or `host:port`.
	pub host: String,
	/// The repository path with no leading slash, e.g. `acme/app.git` — `None` when the URL has none.
	/// Kept **percent-encoded** as it appeared in the URL: git decodes it differently for the two uses,
	/// fully for a helper's `path=` line (`a%20b` → `a b`) but only for the unreserved octets when
	/// matching `credential.<url>` (an encoded `%2F` is *not* a path separator), so the consumer decodes
	/// as each use requires. An encoded NUL (`%00`) is preserved through both.
	pub path: Option<String>,
	/// A known username the credential must be for, if any (URL userinfo or `credential.username`). This is
	/// the **resolution key**: a provider matches a username-scoped `credential.<user>@host` section on it
	/// and settles which helpers/`useHttpPath` apply. git computes that selection once and never re-keys it
	/// on a later round (its `credential_apply_config` `configured` latch), so this stays the *initial*
	/// username across a multistage handshake — see [`carried_username`](Self::carried_username).
	pub username: Option<String>,
	/// The username a previous multistage round *learned* (git's retained `c->username`), to re-present to a
	/// continuation helper and carry onto the resolved credential — kept **separate** from
	/// [`username`](Self::username) so it feeds the helper's `get` and the final `store`/`erase` without
	/// re-keying helper selection. `None` on the first round.
	pub carried_username: Option<String>,
	/// The server's `WWW-Authenticate` challenge values, one per header field, in order — empty unless
	/// this request accompanies a `401` challenge being filled.
	pub wwwauth: Vec<String>,
	/// Opaque `state[]` values a helper returned on a previous round, echoed back so a multistage
	/// authentication (git's `state` capability) can resume — empty on the first round. Each value is
	/// prefixed by the helper that owns it; a helper ignores values that are not its own.
	pub state: Vec<String>,
	/// The authentication scheme (`authtype`) carried from a previous multistage round — `None` on the
	/// first round or after a Basic round. git clears only the secret between rounds, retaining `authtype`,
	/// so the next `fill` re-presents it to a continuation helper.
	pub authtype: Option<String>,
	/// The `ephemeral` flag carried from a previous multistage round — git likewise retains it across the
	/// round, so a helper that completes the negotiation without re-stating `ephemeral` still yields an
	/// ephemeral credential (not persisted). `false` on the first round.
	pub ephemeral: bool,
	/// Whether the `authtype` capability was negotiated in a previous round — git retains a capability's
	/// helper-side bit across rounds, so a continuation helper's `authtype`/`credential` is honoured even
	/// without re-advertising. Carried independently of [`state` cap](Self::caps_state): a round that
	/// negotiated only `state` must not enable `authtype`. `false` on the first round.
	pub caps_authtype: bool,
	/// Whether the `state` capability was negotiated in a previous round — retained like
	/// [`caps_authtype`](Self::caps_authtype). `false` on the first round.
	pub caps_state: bool,
}

impl CredentialRequest {
	/// Derive the request attributes from a Smart HTTP `url` (already userinfo-stripped by
	/// [`Origin`](crate::Origin)) plus an optional `username` hint. Returns `None` for a URL whose
	/// scheme or host cannot be read — an unkeyable request the caller skips (proceeding
	/// unauthenticated) rather than treating as a hard error.
	pub fn from_url(url: &str, username: Option<String>) -> Option<Self> {
		let (protocol, rest) = url.split_once("://")?;
		// The authority runs to the first `/`, `?`, or `#`; the path is what a `/` introduces.
		let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
		let host = &rest[..authority_end];
		if host.is_empty() {
			return None;
		}
		let path = rest[authority_end..]
			.split(['?', '#'])
			.next()
			.unwrap_or("")
			.trim_start_matches('/');
		// The path is kept percent-encoded (see the field doc); the consumer decodes it per use.
		let path = (!path.is_empty()).then(|| path.to_owned());
		let request = Self {
			protocol: protocol.to_owned(),
			host: host.to_owned(),
			path,
			username,
			carried_username: None,
			wwwauth: Vec::new(),
			state: Vec::new(),
			authtype: None,
			ephemeral: false,
			caps_authtype: false,
			caps_state: false,
		};
		// git rejects a credential whose (decoded) attributes contain a newline or carriage return (its
		// `check_url_component`), since a `key=value\n` helper line cannot carry one — a decoded `%0A` in
		// the path must not silently drop the `path=` line and broaden a path-scoped lookup. The path is
		// checked in its fully-decoded form (as a helper receives it). Fail closed: an unkeyable request,
		// so the caller proceeds unauthenticated.
		let has_control = |value: &str| value.contains(['\n', '\r']);
		let decoded_path_has_control = request
			.path
			.as_deref()
			.is_some_and(|path| has_control(&crate::percent_decode(path)));
		if has_control(&request.protocol)
			|| has_control(&request.host)
			|| decoded_path_has_control
			|| request.username.as_deref().is_some_and(has_control)
		{
			return None;
		}
		Some(request)
	}

	/// Attach the server's `WWW-Authenticate` challenge values (one per header field), returning `self`
	/// so a caller can key an otherwise-identical request with the challenge that provoked it. Used by
	/// [`AuthTransport`](crate::AuthTransport) to carry the `401`'s challenge into a
	/// [`fill`](crate::CredentialProvider::fill) so a helper receives it as `wwwauth[]`.
	pub fn with_wwwauth(mut self, wwwauth: Vec<String>) -> Self {
		self.wwwauth = wwwauth;
		self
	}

	/// Attach the `state[]` values a helper returned last round (git's `state` capability), returning
	/// `self`, so the next [`fill`](crate::CredentialProvider::fill) resumes a multistage authentication.
	pub fn with_state(mut self, state: Vec<String>) -> Self {
		self.state = state;
		self
	}

	/// Carry the previous multistage round's credential context — its `authtype` and `ephemeral` flag —
	/// into the next [`fill`](crate::CredentialProvider::fill), mirroring git:
	/// [`credential_clear_secrets`](https://github.com/git/git/blob/master/credential.c) drops only the
	/// secret between rounds, retaining these attributes so a continuation helper resumes the same scheme
	/// and the `ephemeral` marker is not lost when the completing helper omits it.
	///
	/// The resolution-key [`username`](Self::username) is deliberately *not* overridden with the round's
	/// resolved value: git computes helper selection and `useHttpPath` exactly once (its
	/// `credential_apply_config` `configured` latch), so a username a helper *learned* in round one must not
	/// re-key config resolution and swap the helper chain mid-negotiation. The learned username instead
	/// rides [`carried_username`](Self::carried_username) — re-presented to the continuation helper and
	/// carried onto the final credential — while the original URL-userinfo hint holds resolution stable, as
	/// git does.
	///
	/// `caps_authtype`/`caps_state` are the capabilities the previous round negotiated; git retains each
	/// capability's helper-side bit across the round independently, so a continuation helper's
	/// capability-gated fields are honoured without re-advertising. `username`/`authtype`/`ephemeral` are
	/// read straight off the round's [`Credential`](crate::Credential) — git keeps each as its own field
	/// and retains it across the round, so an `authtype` survives even when the round's credential was a
	/// plain username/password. `state` is threaded separately (see [`with_state`](Self::with_state)).
	pub fn with_credential_context(mut self, previous: &crate::Filled) -> Self {
		self.carried_username = previous.credential.username.clone();
		self.authtype = previous.credential.authtype.clone();
		self.ephemeral = previous.credential.ephemeral;
		self.caps_authtype = previous.caps_authtype;
		self.caps_state = previous.caps_state;
		self
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Credential, Filled};

	#[test]
	fn carries_authtype_from_a_basic_credential_round() {
		// A multistage round can complete with a username/password credential yet still negotiate an
		// `authtype` (git keeps it as its own field and retains it across the round). The flat credential
		// holds `username`, `password`, and `authtype` together, so the carried context re-presents the
		// scheme — else a continuation helper cannot resume the negotiation.
		let previous = Filled {
			credential: Credential {
				username: Some("alice".to_owned()),
				password: Some("pw".to_owned()),
				authtype: Some("negotiate".to_owned()),
				..Credential::default()
			},
			state: vec!["s1".to_owned()],
			more: true,
			caps_authtype: true,
			caps_state: true,
		};
		let request = CredentialRequest::from_url("https://example.com/app.git", None)
			.unwrap()
			.with_credential_context(&previous);
		assert_eq!(request.authtype.as_deref(), Some("negotiate"));
		assert_eq!(request.carried_username.as_deref(), Some("alice"));
		assert!(request.caps_authtype && request.caps_state);
	}

	#[test]
	fn parses_protocol_host_and_path() {
		let req = CredentialRequest::from_url("https://example.com/acme/app.git", None).unwrap();
		assert_eq!(req.protocol, "https");
		assert_eq!(req.host, "example.com");
		assert_eq!(req.path.as_deref(), Some("acme/app.git"));
		assert_eq!(req.username, None);
	}

	#[test]
	fn keeps_port_in_host_and_carries_username_hint() {
		let req =
			CredentialRequest::from_url("http://localhost:8080/repo", Some("alice".to_owned())).unwrap();
		assert_eq!(req.host, "localhost:8080");
		assert_eq!(req.path.as_deref(), Some("repo"));
		assert_eq!(req.username.as_deref(), Some("alice"));
	}

	#[test]
	fn drops_query_and_handles_no_path() {
		assert_eq!(
			CredentialRequest::from_url("https://host/info/refs?service=git-upload-pack", None)
				.unwrap()
				.path
				.as_deref(),
			Some("info/refs")
		);
		assert_eq!(
			CredentialRequest::from_url("https://host", None)
				.unwrap()
				.path,
			None
		);
	}

	#[test]
	fn rejects_an_unkeyable_url() {
		assert!(CredentialRequest::from_url("not-a-url", None).is_none());
		assert!(CredentialRequest::from_url("https://", None).is_none());
	}

	#[test]
	fn rejects_a_control_character_in_a_decoded_path() {
		// A `%0A` decodes to a newline; git rejects such a credential rather than letting a `path=`
		// line be silently dropped and a path-scoped lookup broaden to the whole host.
		assert!(CredentialRequest::from_url("https://host/a%0Ab", None).is_none());
		assert!(CredentialRequest::from_url("https://host/a%0Db", None).is_none());
		// The plain (escape-free) path is unaffected.
		assert_eq!(
			CredentialRequest::from_url("https://host/a/b", None)
				.unwrap()
				.path
				.as_deref(),
			Some("a/b")
		);
	}
}
