//! A resolved HTTP credential.

/// A resolved HTTP credential, modelled as git's flat `struct credential` (git-credential(5)): the four
/// value attributes plus `ephemeral`. `username`/`password` are the Basic pair; `authtype`/`credential`
/// (git ≥ 2.42) are a scheme name (`bearer`, `digest`, …) plus a pre-encoded value a helper returns
/// under the `authtype` capability. Per git, when `credential` is present `authtype` is mandatory and
/// `username`/`password` are not used *for the header* — but git retains every populated attribute (they
/// are handed back to a helper on `store`/`erase`), so all four are kept rather than made mutually
/// exclusive by the type. What git's credential machinery fills and what
/// [`AuthTransport`](crate::AuthTransport) turns into an `Authorization` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
	/// The account name (git's `username`) — the Basic username, and the identity git retains across a
	/// multistage round and hands to a helper on `store`/`erase`. `None` when unknown.
	pub username: Option<String>,
	/// The Basic secret (git's `password`) — an account password, PAT, or app password. `None` when the
	/// credential authenticates via `authtype`/`credential` instead.
	pub password: Option<String>,
	/// The authentication scheme (git's `authtype`): `bearer`, `digest`, `basic`, … `None` for a plain
	/// Basic credential (the scheme is implicit). Retained across a multistage round even without a
	/// `credential`, mirroring git's own field.
	pub authtype: Option<String>,
	/// The pre-encoded credential value (git's `credential`) — paired with [`authtype`](Self::authtype) to
	/// form the header `<authtype> <credential>` (e.g. `Bearer <token>`). `None` for a plain Basic
	/// credential.
	pub credential: Option<String>,
	/// Whether the credential is short-lived and must not be saved by a helper (git's `ephemeral`).
	pub ephemeral: bool,
}

impl Credential {
	/// A non-ephemeral HTTP Basic credential — the common case (URL userinfo, a `store`-format helper, a
	/// prompt).
	pub fn basic(username: String, password: String) -> Self {
		Self {
			username: Some(username),
			password: Some(password),
			..Self::default()
		}
	}

	/// A non-ephemeral pre-encoded credential — git's `authtype`+`credential` (Bearer/Digest/…), with no
	/// account attributes. Convenience for the common encoded case; set the fields directly for a credential
	/// that also carries a `username`/`password` or is `ephemeral`.
	pub fn encoded(authtype: String, credential: String) -> Self {
		Self {
			authtype: Some(authtype),
			credential: Some(credential),
			..Self::default()
		}
	}

	/// Whether the credential authenticates via a pre-encoded `authtype`+`credential` (**both** present, as
	/// git requires — `credential` alone is malformed and cannot form an encoded header). The single shape
	/// decision [`auth_header`](Self::auth_header) and [`is_basic`](Self::is_basic) must agree on, so the
	/// Basic-disclosure gate and the header can never disagree.
	fn is_encoded(&self) -> bool {
		self.authtype.is_some() && self.credential.is_some()
	}

	/// Whether this credential is sent as a base64 **Basic** secret — so it may go only to a challenge that
	/// offers Basic, never to a Bearer/Negotiate-only server. True whenever the header is not encoded (it
	/// falls back to `Basic base64(user:pass)` — including a malformed `credential` with no `authtype`), or
	/// when a capability-aware helper pre-encoded the Basic secret as `authtype=basic` + `credential`. Kept
	/// in lockstep with [`auth_header`](Self::auth_header) via [`is_encoded`](Self::is_encoded).
	pub fn is_basic(&self) -> bool {
		!self.is_encoded()
			|| self
				.authtype
				.as_deref()
				.is_some_and(|authtype| authtype.eq_ignore_ascii_case("basic"))
	}

	/// The account name git retains across a multistage round to re-present to a continuation helper (git's
	/// `c->username`) — just the [`username`](Self::username) field, named for the call site.
	pub fn username(&self) -> Option<&str> {
		self.username.as_deref()
	}

	/// The `Authorization` header value. When an encoded [`credential`](Self::credential) is present git
	/// concatenates `<authtype> <credential>` (e.g. `Bearer <token>`); otherwise it is
	/// `Basic base64(user:pass)` (RFC 7617 base64 of the raw `user:pass` bytes, exactly as git via libcurl
	/// sends it). A missing `username`/`password` encodes as the empty string, matching git's empty-auth.
	pub fn auth_header(&self) -> String {
		match (self.is_encoded(), &self.authtype, &self.credential) {
			(true, Some(authtype), Some(credential)) => format!("{authtype} {credential}"),
			// Not encoded (or a malformed credential-without-authtype) — the Basic header, matching what
			// `is_basic` gates on. A missing username/password encodes as the empty string (git's empty-auth).
			_ => {
				let username = self.username.as_deref().unwrap_or_default();
				let password = self.password.as_deref().unwrap_or_default();
				format!(
					"Basic {}",
					base64_encode(format!("{username}:{password}").as_bytes())
				)
			}
		}
	}
}

/// Standard base64 (RFC 4648, `+`/`/`, `=` padding) of `input`. Basic auth needs only encoding, and a
/// dozen lines keep gitana free of a base64 dependency in its wasm-facing remote crate (there is none
/// in the workspace today).
fn base64_encode(input: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
	for chunk in input.chunks(3) {
		// Pack up to three bytes into a 24-bit big-endian group, tracking how many were real.
		let b0 = chunk[0] as u32;
		let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
		let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
		let group = (b0 << 16) | (b1 << 8) | b2;
		out.push(ALPHABET[(group >> 18) as usize & 0x3f] as char);
		out.push(ALPHABET[(group >> 12) as usize & 0x3f] as char);
		// The third and fourth sextets are real only when their source bytes were present, else `=`.
		out.push(if chunk.len() > 1 {
			ALPHABET[(group >> 6) as usize & 0x3f] as char
		} else {
			'='
		});
		out.push(if chunk.len() > 2 {
			ALPHABET[group as usize & 0x3f] as char
		} else {
			'='
		});
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn base64_matches_rfc4648_vectors() {
		// The canonical RFC 4648 §10 test vectors, covering both padding lengths.
		for (input, expected) in [
			("", ""),
			("f", "Zg=="),
			("fo", "Zm8="),
			("foo", "Zm9v"),
			("foob", "Zm9vYg=="),
			("fooba", "Zm9vYmE="),
			("foobar", "Zm9vYmFy"),
		] {
			assert_eq!(base64_encode(input.as_bytes()), expected, "for {input:?}");
		}
	}

	#[test]
	fn basic_auth_header_encodes_user_and_password() {
		// The classic RFC 7617 example: `Aladdin:open sesame` → `QWxhZGRpbjpvcGVuIHNlc2FtZQ==`.
		let cred = Credential::basic("Aladdin".to_owned(), "open sesame".to_owned());
		assert_eq!(cred.auth_header(), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
	}

	#[test]
	fn encoded_auth_header_is_scheme_space_value() {
		// git's `authtype`+`credential` HTTP form: `<authtype> <credential>`.
		let cred = Credential {
			ephemeral: true,
			..Credential::encoded("Bearer".to_owned(), "xyz.token".to_owned())
		};
		assert_eq!(cred.auth_header(), "Bearer xyz.token");
	}

	#[test]
	fn is_basic_distinguishes_the_wire_shapes() {
		// A plain username/password credential (no encoded credential) is Basic.
		assert!(Credential::basic("u".to_owned(), "p".to_owned()).is_basic());
		// An encoded Bearer credential is not Basic — it must not be sent to a Basic-only-expecting gate.
		assert!(!Credential::encoded("bearer".to_owned(), "tok".to_owned()).is_basic());
		// A capability-aware helper may pre-encode the Basic secret as `authtype=basic` — still Basic.
		assert!(Credential::encoded("basic".to_owned(), "dXNlcjpwYXNz".to_owned()).is_basic());
	}

	#[test]
	fn a_credential_without_authtype_is_basic_and_never_leaks_a_bearer_header() {
		// A malformed credential (a `credential` with no `authtype`, plus username/password) cannot form an
		// encoded header — `auth_header` falls back to Basic, so `is_basic` MUST agree (else the base64
		// user:pass would slip past the Bearer-only gate and disclose to a non-Basic server).
		let malformed = Credential {
			username: Some("alice".to_owned()),
			password: Some("pw".to_owned()),
			authtype: None,
			credential: Some("stray".to_owned()),
			..Credential::default()
		};
		assert!(malformed.is_basic(), "must be gated as Basic");
		assert!(
			malformed.auth_header().starts_with("Basic "),
			"header is Basic, so the gate must match"
		);
	}
}
