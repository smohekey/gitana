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
	/// A known username the credential must be for, if any.
	pub username: Option<String>,
	/// The server's `WWW-Authenticate` challenge values, one per header field, in order — empty unless
	/// this request accompanies a `401` challenge being filled.
	pub wwwauth: Vec<String>,
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
			wwwauth: Vec::new(),
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
}

#[cfg(test)]
mod tests {
	use super::*;

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
