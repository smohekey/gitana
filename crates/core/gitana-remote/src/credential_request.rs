//! A credential lookup key, derived from the remote URL.

/// The context a [`CredentialProvider`](crate::CredentialProvider) resolves a credential for — git's
/// credential attributes. `protocol` and `host` always identify the server; `path` is the repository
/// path (git passes it only when `credential.useHttpPath` is set, so the provider decides whether to
/// key on it); `username` is a known username the credential must match (from URL userinfo or
/// `credential.username`), narrowing the lookup and pre-filling a helper/prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
	/// The URL scheme, e.g. `https`.
	pub protocol: String,
	/// The authority, `host` or `host:port`.
	pub host: String,
	/// The repository path with no leading slash, e.g. `acme/app.git` — `None` when the URL has none.
	pub path: Option<String>,
	/// A known username the credential must be for, if any.
	pub username: Option<String>,
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
		let path = (!path.is_empty()).then(|| path.to_owned());
		Some(Self {
			protocol: protocol.to_owned(),
			host: host.to_owned(),
			path,
			username,
		})
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
}
