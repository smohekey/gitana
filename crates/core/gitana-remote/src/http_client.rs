//! The raw HTTP client seam for Git Smart HTTP remotes.
//!
//! This is the byte-level transport [`HttpTransport`](crate::HttpTransport) is layered on: it
//! surfaces the response **status** and accepts arbitrary request **headers**, the two things the
//! credential flow needs (see [`AuthTransport`](crate::AuthTransport)) that the body-returning
//! transport deliberately hides from the porcelain above it. A raw client makes **no** decision
//! about authentication or about non-2xx status — it reports what the server said and lets the layer
//! above act on it. Only a genuine transport failure (DNS, TLS, a dropped connection) is an `Err`.

use std::future::Future;

use anyhow::Result;

/// One HTTP response: the status code, the `WWW-Authenticate` challenge(s) (if any), and the complete
/// body. Smart HTTP v0 as gitana speaks it is request → whole response (the pack response is parsed in
/// full, no sideband interleave), so the body is always read to completion — a raw client never
/// streams. The challenge is surfaced so [`AuthTransport`](crate::AuthTransport) only offers Basic
/// credentials when the server actually asked for Basic (git does not send Basic to a Bearer/Negotiate
/// realm).
#[derive(Debug, Clone)]
pub struct HttpResponse {
	/// The HTTP status code (e.g. `200`, `401`).
	pub status: u16,
	/// The `WWW-Authenticate` header values on a `401`, one per header field the server sent, in order
	/// (e.g. `["Basic realm=\"…\""]`, or `["Negotiate", "Basic realm=\"…\""]` when split across fields).
	/// Kept unjoined so each is passed to a credential helper as a distinct `wwwauth[]` line, as git does.
	pub www_authenticate: Vec<String>,
	/// The complete response body.
	pub body: Vec<u8>,
}

impl HttpResponse {
	/// Whether the status is 2xx.
	pub fn is_success(&self) -> bool {
		(200..300).contains(&self.status)
	}

	/// Whether a `401`'s `WWW-Authenticate` challenge offers HTTP Basic — the only scheme gitana speaks.
	/// A challenge may list several schemes (and auth-params) separated by commas; the schemes are split
	/// on commas **outside** quoted strings, so a comma inside a quoted `realm` (e.g. `Digest
	/// realm="tenant, Basic admin"`) is not mistaken for a Basic offer. A scheme is the first token of a
	/// segment; matching is case-insensitive, per RFC 7235.
	pub fn offers_basic_auth(&self) -> bool {
		self.www_authenticate.iter().any(|challenge| {
			split_outside_quotes(challenge).into_iter().any(|segment| {
				segment
					.trim()
					.split_whitespace()
					.next()
					.is_some_and(|token| token.eq_ignore_ascii_case("basic"))
			})
		})
	}

	/// Consume the response into its body when the status is 2xx, else the same error the transports
	/// raised before status was surfaced: `"{url}: HTTP {status}: {body}"`. Both the body-returning
	/// [`HttpTransport`](crate::HttpTransport) wrappers end here, so anonymous callers see identical
	/// non-2xx errors to today.
	pub fn into_body(self, url: &str) -> Result<Vec<u8>> {
		if self.is_success() {
			Ok(self.body)
		} else {
			anyhow::bail!(
				"{url}: HTTP {}: {}",
				self.status,
				String::from_utf8_lossy(&self.body)
			);
		}
	}
}

/// Split `header` on commas that are **not** inside a double-quoted string (respecting `\`-escapes),
/// as an `#`-list is delimited in RFC 7230/7235 — so a comma within a quoted `realm` does not split.
fn split_outside_quotes(header: &str) -> Vec<&str> {
	let mut segments = Vec::new();
	let mut in_quotes = false;
	let mut escaped = false;
	let mut start = 0;
	for (i, ch) in header.char_indices() {
		if escaped {
			escaped = false;
		} else if in_quotes && ch == '\\' {
			escaped = true;
		} else if ch == '"' {
			in_quotes = !in_quotes;
		} else if ch == ',' && !in_quotes {
			segments.push(&header[start..i]);
			start = i + 1;
		}
	}
	segments.push(&header[start..]);
	segments
}

/// A minimal, auth-agnostic HTTP client for Git Smart HTTP: `GET` (ref advertisements) and `POST`
/// (`git-upload-pack` / `git-receive-pack`). Each method forwards the caller's request `headers`
/// (e.g. an `Authorization`) verbatim and returns the [`HttpResponse`] — status **and** body — with
/// no non-2xx handling of its own. The credential-aware [`AuthTransport`](crate::AuthTransport)
/// wraps a client to implement git's 401-retry flow; the plain
/// [`Unauthenticated`](crate::Unauthenticated) wrapper restores the "non-2xx is an error" behaviour
/// for anonymous callers. Both present the tiny [`HttpTransport`](crate::HttpTransport) the porcelain
/// consumes, keeping this client dumb.
///
/// The seam is small enough that a synchronous, pollable-free implementation (an in-guest
/// `wasi:http` client on `wasm32-wasip2`) satisfies it alongside the native
/// [`ReqwestTransport`](crate::ReqwestTransport) (behind the default `reqwest-transport` feature).
pub trait HttpClient {
	/// `GET` `url` with `headers`, returning the status and complete body.
	fn get(
		&self,
		url: &str,
		headers: &[(String, String)],
	) -> impl Future<Output = Result<HttpResponse>>;

	/// `POST` `body` with `content_type` and `headers` to `url`, returning the status and complete body.
	fn post(
		&self,
		url: &str,
		content_type: &str,
		body: Vec<u8>,
		headers: &[(String, String)],
	) -> impl Future<Output = Result<HttpResponse>>;
}

#[cfg(test)]
mod tests {
	use super::*;

	fn challenge(www_authenticate: Option<&str>) -> HttpResponse {
		HttpResponse {
			status: 401,
			www_authenticate: www_authenticate.map(str::to_owned).into_iter().collect(),
			body: Vec::new(),
		}
	}

	#[test]
	fn detects_a_basic_challenge_case_insensitively_among_schemes() {
		assert!(challenge(Some("Basic realm=\"x\"")).offers_basic_auth());
		assert!(challenge(Some("basic realm=\"x\"")).offers_basic_auth());
		// A multi-scheme challenge that includes Basic still counts.
		assert!(challenge(Some("Negotiate, Basic realm=\"x\"")).offers_basic_auth());
	}

	#[test]
	fn rejects_a_non_basic_or_absent_challenge() {
		assert!(!challenge(Some("Bearer realm=\"x\"")).offers_basic_auth());
		assert!(!challenge(Some("Negotiate")).offers_basic_auth());
		assert!(!challenge(None).offers_basic_auth());
	}

	#[test]
	fn a_quoted_comma_realm_is_not_a_basic_offer() {
		// The `Basic` here is inside a quoted realm of a Digest challenge — not a scheme offer.
		assert!(!challenge(Some("Digest realm=\"tenant, Basic admin\"")).offers_basic_auth());
		// But a genuine second Basic scheme after a quoted realm still counts.
		assert!(challenge(Some("Digest realm=\"a, b\", Basic realm=\"x\"")).offers_basic_auth());
	}

	#[test]
	fn detects_basic_split_across_separate_header_fields() {
		// git surfaces one `WWW-Authenticate` field per element; a Basic offer in any field counts.
		let response = HttpResponse {
			status: 401,
			www_authenticate: vec!["Negotiate".to_owned(), "Basic realm=\"x\"".to_owned()],
			body: Vec::new(),
		};
		assert!(response.offers_basic_auth());
	}
}
