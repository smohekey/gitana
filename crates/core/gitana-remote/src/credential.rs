//! A resolved HTTP credential.

/// A username/password pair resolved for an HTTP remote — what git's credential machinery fills and
/// what [`AuthTransport`](crate::AuthTransport) turns into an `Authorization: Basic` header. A
/// "password" here is whatever secret the scheme carries: an account password, a personal access
/// token, or an app password (GitHub, GitLab, and Bitbucket all accept a token in this field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
	/// The username.
	pub username: String,
	/// The secret (password / token).
	pub password: String,
}

impl Credential {
	/// The `Authorization` header value for HTTP Basic auth: `Basic base64(username:password)`, exactly
	/// as git (via libcurl) sends it. RFC 7617 base64-encodes the raw `user:pass` bytes.
	pub fn basic_auth_header(&self) -> String {
		format!(
			"Basic {}",
			base64_encode(format!("{}:{}", self.username, self.password).as_bytes())
		)
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
		let cred = Credential {
			username: "Aladdin".to_owned(),
			password: "open sesame".to_owned(),
		};
		assert_eq!(
			cred.basic_auth_header(),
			"Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
		);
	}
}
