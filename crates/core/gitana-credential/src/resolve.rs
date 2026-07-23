//! Resolving the credential config that applies to a request: which helpers to run, plus the
//! `username` and `useHttpPath` git would settle on — following git's matching and precedence exactly.
//!
//! git walks `credential.*` in config order through `urlmatch_config_entry` with `select_fn =
//! select_all` (`credential.c`), so for credentials there is **no specificity ranking**: every entry
//! whose `credential.<url>` subsection matches the request applies, in read order. A single-valued key
//! (`username`, `useHttpPath`) is therefore plain last-writer-wins; `helper` accumulates, and an empty
//! `helper=` resets the list. The section-level `credential.<key>` (no subsection) always applies.

use anyhow::{Result, bail};
use gitana_config::GitConfig;
use gitana_remote::CredentialRequest;

use crate::helper::Helper;

/// The resolved credential configuration for one request: the ordered helper chain, the configured
/// username (before the URL-userinfo hint is considered), and whether the repository path is part of
/// the credential's identity.
pub(crate) struct CredentialConfig {
	pub helpers: Vec<Helper>,
	pub username: Option<String>,
	pub use_http_path: bool,
}

/// Resolve the [`CredentialConfig`] for `request` from `config`, following git's matching and
/// precedence. Errors on a valueless `credential.*` entry, as git rejects a non-boolean config key
/// with no value.
pub(crate) fn resolve(config: &GitConfig, request: &CredentialRequest) -> Result<CredentialConfig> {
	let mut helpers = Vec::new();
	for (subsection, value) in config.variables_named("credential", "helper") {
		if !applies(subsection, request) {
			continue;
		}
		match value {
			// An empty `helper=` resets the accumulated list (git: `if (*value) append else clear`).
			Some("") => helpers.clear(),
			Some(value) => helpers.push(Helper::parse(value)),
			None => bail!("missing value for 'credential.helper'"),
		}
	}

	// A single-valued key: the last matching entry in read order wins (select_all → no specificity).
	let mut username = None;
	for (subsection, value) in config.variables_named("credential", "username") {
		if !applies(subsection, request) {
			continue;
		}
		match value {
			Some(value) => username = Some(value.to_owned()),
			None => bail!("missing value for 'credential.username'"),
		}
	}

	let mut use_http_path = false;
	for (subsection, value) in config.variables_named("credential", "usehttppath") {
		if !applies(subsection, request) {
			continue;
		}
		match value {
			Some(value) => {
				use_http_path = gitana_config_native::parse_git_bool(value).ok_or_else(|| {
					anyhow::anyhow!("bad boolean value '{value}' for 'credential.useHttpPath'")
				})?
			}
			None => bail!("missing value for 'credential.useHttpPath'"),
		}
	}

	Ok(CredentialConfig {
		helpers,
		username,
		use_http_path,
	})
}

/// Whether a `credential.<subsection>` config entry applies to `request`: a section-level entry (no
/// subsection) always does; a `credential.<url>` entry does only when its URL pattern matches.
fn applies(subsection: Option<&str>, request: &CredentialRequest) -> bool {
	match subsection {
		None => true,
		Some(pattern) => subsection_matches(pattern, request),
	}
}

/// Whether a `credential.<pattern>` URL pattern matches `request`, by git's rules. A pattern that
/// parses as a full URL (scheme **and** host) matches via git's `urlmatch` (`urlmatch.c`
/// `match_urls`): scheme exact, an optional userinfo exact, host by label with `*` wildcarding a
/// single label, port equal after default-port stripping, and path a prefix ending on a `/` boundary.
/// A pattern that is not a full URL (git's `url_normalize` fails — e.g. a scheme-less `example.com`)
/// falls back to git's partial match (`credential.c` `credential_match`): each component the pattern
/// *does* specify must equal the request's exactly, and the rest is unconstrained.
fn subsection_matches(pattern: &str, request: &CredentialRequest) -> bool {
	match FullPattern::parse(pattern) {
		Some(full) => full.matches(request),
		None => PartialPattern::parse(pattern).matches(request),
	}
}

/// A `credential.<url>` pattern parsed as a full URL (has a scheme and a host), normalised the way
/// git's `url_normalize` does for the fields matching depends on.
struct FullPattern {
	/// Lower-cased scheme, e.g. `https`.
	scheme: String,
	/// The userinfo username, if the pattern carried one (`https://user@host`).
	user: Option<String>,
	/// Lower-cased host with no port, e.g. `example.com` (may contain `*` wildcard labels).
	host: String,
	/// The port, or `None` when absent or equal to the scheme's default.
	port: Option<String>,
	/// The path with a leading `/` (git's rule 7), `"/"` when the pattern had none.
	path: String,
}

impl FullPattern {
	/// Parse `pattern` as a full URL, or `None` when it has no scheme or no host (git's `url_normalize`
	/// would reject it, so matching falls back to the partial path).
	fn parse(pattern: &str) -> Option<Self> {
		let (scheme, rest) = pattern.split_once("://")?;
		if scheme.is_empty() {
			return None;
		}
		// git's `url_normalize` handles only scheme://host/path — a query or fragment makes it fail, so
		// such a pattern is not a full URL (it falls through to the partial match, which then constrains
		// the path to the `?…`/`#…` literal and matches nothing normal, exactly as git ends up ignoring it).
		if rest.contains(['?', '#']) {
			return None;
		}
		let scheme = scheme.to_ascii_lowercase();
		let authority_end = rest.find('/').unwrap_or(rest.len());
		let authority = &rest[..authority_end];
		let (user, host_port) = match authority.rsplit_once('@') {
			Some((userinfo, host_port)) => {
				// A present userinfo is a username constraint even when empty (`https://:secret@host`):
				// git requires an exact match, so the empty username matches no ordinary request rather
				// than dropping the constraint and broadening to a host-wide pattern. Decoded, as git
				// decodes the userinfo before comparing (the request username is decoded too).
				let user = userinfo.split(':').next().unwrap_or("");
				(Some(gitana_remote::percent_decode(user)), host_port)
			}
			None => (None, authority),
		};
		let (host, port) = split_host_port(host_port);
		if host.is_empty() {
			return None;
		}
		// git's `url_normalize` fails on a path it cannot canonicalise — a malformed `%XX` escape or an
		// encoded NUL — falling back to exact partial matching (so `.../a%zz` matches only an exact `a%zz`,
		// not as a prefix). Reject such a pattern here so it takes the partial path.
		if path_has_unnormalizable_escape(&rest[authority_end..]) {
			return None;
		}
		// Normalise as git's `url_normalize` does — decoding only unreserved escapes and preserving a
		// reserved one like `%2F` — so a `%2F` pattern does not match a literal `/`. No query/fragment
		// remains (rejected above).
		let path = normalize_url_path(&rest[authority_end..]);
		let path = if path.is_empty() {
			"/".to_owned()
		} else if path.starts_with('/') {
			path
		} else {
			format!("/{path}")
		};
		Some(Self {
			port: normalize_port(&scheme, port),
			scheme,
			user,
			host: normalize_host(&host),
			path,
		})
	}

	/// Whether this full-URL pattern matches `request`, following `match_urls`.
	fn matches(&self, request: &CredentialRequest) -> bool {
		let scheme = request.protocol.to_ascii_lowercase();
		if self.scheme != scheme {
			return false;
		}
		if let Some(user) = &self.user
			&& request.username.as_deref() != Some(user.as_str())
		{
			return false;
		}
		let (req_host, req_port) = split_host_port(&request.host);
		if !host_matches(&normalize_host(&req_host), &self.host) {
			return false;
		}
		// The lower-cased scheme drives default-port stripping, so `HTTPS://host:443` recognises 443 as
		// the default (matching `self.port`, which was normalised against the already-lower-cased scheme).
		if self.port != normalize_port(&scheme, req_port) {
			return false;
		}
		path_prefix_matches(&normalize_request_path(request.path.as_deref()), &self.path)
	}
}

/// A `credential.<url>` pattern that is not a full URL (no scheme), parsed by git's partial rules
/// (`credential.c` `credential_from_url_1` with `allow_partial_url`). Only the components present are
/// constraints, each matched exactly (`credential_match`).
struct PartialPattern {
	protocol: Option<String>,
	host: Option<String>,
	username: Option<String>,
	path: Option<String>,
}

impl PartialPattern {
	fn parse(pattern: &str) -> Self {
		let (protocol, rest) = match pattern.split_once("://") {
			Some((scheme, rest)) if !scheme.is_empty() => (Some(scheme.to_owned()), rest),
			_ => (None, pattern),
		};
		let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
		let authority = &rest[..authority_end];
		let (username, host) = match authority.rsplit_once('@') {
			Some((userinfo, host)) => {
				// A present userinfo constrains the username even when empty (see `FullPattern::parse`),
				// decoded as git decodes it.
				let user = userinfo.split(':').next().unwrap_or("");
				(Some(gitana_remote::percent_decode(user)), host)
			}
			None => (None, authority),
		};
		// git's partial parse keeps everything after the authority as the path — including a `?`/`#`, which
		// it does not strip (`credential_from_url_1` only trims slashes). So a `host?q` pattern constrains
		// the path to `?q` and matches no ordinary request path, rather than degrading to a host-only match.
		let path = gitana_remote::percent_decode(rest[authority_end..].trim_matches('/'));
		Self {
			protocol,
			// git's partial parse fully decodes the host too (`credential_from_url_1`).
			host: (!host.is_empty()).then(|| gitana_remote::percent_decode(host)),
			username,
			path: (!path.is_empty()).then_some(path),
		}
	}

	fn matches(&self, request: &CredentialRequest) -> bool {
		// git never matches an encoded NUL, even in the partial fallback (`percent_decode` preserves
		// `%00`, so a path carrying one is checked here).
		if self
			.path
			.as_deref()
			.is_some_and(|path| path.contains("%00"))
		{
			return false;
		}
		let exact = |pattern: &Option<String>, value: Option<&str>| match pattern {
			Some(pattern) => value == Some(pattern.as_str()),
			None => true,
		};
		// The partial fallback is git's `credential_from_url_1`, which *fully* decodes the host and path
		// (unlike full-URL matching's `url_normalize`); compare the request decoded the same way.
		let decoded_host = gitana_remote::percent_decode(&request.host);
		let decoded_path = request.path.as_deref().map(gitana_remote::percent_decode);
		exact(&self.protocol, Some(&request.protocol))
			&& exact(&self.host, Some(&decoded_host))
			&& exact(&self.username, request.username.as_deref())
			&& exact(&self.path, decoded_path.as_deref())
	}
}

/// Split an authority into `(host, port)`, keeping an IPv6 literal's brackets and treating a trailing
/// `:<digits>` as the port. An empty port (a bare trailing `:`) normalises to no port, as git does; a
/// missing colon (or a non-numeric tail) also means no port.
fn split_host_port(authority: &str) -> (String, Option<String>) {
	if let Some(rest) = authority.strip_prefix('[')
		&& let Some(close) = rest.find(']')
	{
		let host = format!("[{}]", &rest[..close]);
		return match &rest[close + 1..] {
			"" => (host, None),
			suffix if suffix.starts_with(':') && suffix[1..].bytes().all(|b| b.is_ascii_digit()) => {
				(host, (suffix.len() > 1).then(|| suffix[1..].to_owned()))
			}
			// A non-`:port` suffix after `]` (e.g. `[::1]evil`) is malformed: keep the whole authority as
			// the host so the pattern does not match the canonical bracketed host, as git declines it.
			_ => (authority.to_owned(), None),
		};
	}
	match authority.rsplit_once(':') {
		Some((host, "")) => (host.to_owned(), None),
		Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => {
			(host.to_owned(), Some(port.to_owned()))
		}
		_ => (authority.to_owned(), None),
	}
}

/// Whether `byte` is an RFC 3986 unreserved octet (`A-Za-z0-9-._~`) — the set git's `url_normalize`
/// decodes a `%XX` escape of, in both hosts and paths.
fn is_unreserved(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Whether `byte` is left literal in a `url_normalize`d path: an unreserved octet or a reserved
/// delimiter (RFC 3986 gen/sub-delims). git's `append_normalized_escapes` keeps these literal (it only
/// escapes the unsafe set — control, high, `" <>"%{}|\^`` — and re-escapes an *already-escaped*
/// reserved char), so a literal `:`/`@`/`/`/`[` in a **pattern** stays literal.
fn is_reserved_or_unreserved(byte: u8) -> bool {
	is_unreserved(byte)
		|| matches!(
			byte,
			b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@'  // gen-delims
			| b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' // sub-delims
		)
}

/// Percent-encode `bytes` as git's `credential_format` does when it rebuilds a request URL from the
/// decoded path (`strbuf_add_percentencode` with `URL_UNSAFE_CHARS` and slashes kept): every byte
/// outside the unreserved set is `%XX`-escaped **except** `/`. This is stricter than `url_normalize`'s
/// pattern encoding — it escapes a reserved `:`/`@` too — so a request path and a pattern only match
/// when spelled the same way (git decodes then re-encodes the request, so `a%2Fb` → `a/b`, `a%00b` →
/// `a%2500b`, and a raw `0xFF` byte → `%FF`).
pub fn percent_encode_request_path(bytes: &[u8]) -> String {
	const HEX: &[u8; 16] = b"0123456789ABCDEF";
	let mut out = String::with_capacity(bytes.len());
	for &byte in bytes {
		if byte == b'/' || is_unreserved(byte) {
			out.push(byte as char);
		} else {
			out.push('%');
			out.push(HEX[(byte >> 4) as usize] as char);
			out.push(HEX[(byte & 0xf) as usize] as char);
		}
	}
	out
}

/// Whether `path` contains an escape git's `url_normalize` cannot process: a malformed `%XX` (not two
/// hex digits) or an encoded NUL (`%00`). Such a full-URL pattern fails normalization and falls back to
/// exact partial matching — and git never matches an encoded NUL even there.
fn path_has_unnormalizable_escape(path: &str) -> bool {
	let bytes = path.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			match bytes
				.get(i + 1)
				.zip(bytes.get(i + 2))
				.map(|(hi, lo)| ((*hi as char).to_digit(16), (*lo as char).to_digit(16)))
			{
				// A `%` not followed by two hex digits is malformed.
				None | Some((None, _)) | Some((_, None)) => return true,
				// `%00` is an encoded NUL git refuses to normalise.
				Some((Some(0), Some(0))) => return true,
				Some(_) => i += 3,
			}
		} else {
			i += 1;
		}
	}
	false
}

/// Decode a `%XX` escape of an unreserved octet, keeping every other byte and escape verbatim (with
/// upper-cased hex) — git's `url_normalize` for a component that carries no reserved-byte encoding of
/// its own, such as a host (`exam%70le.com` → `example.com`, while `[::1]` is untouched).
fn decode_unreserved(value: &str) -> String {
	let bytes = value.as_bytes();
	let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& i + 2 < bytes.len()
			&& let (Some(hi), Some(lo)) = (
				(bytes[i + 1] as char).to_digit(16),
				(bytes[i + 2] as char).to_digit(16),
			) {
			let byte = (hi * 16 + lo) as u8;
			if is_unreserved(byte) {
				out.push(byte);
			} else {
				out.push(b'%');
				out.push(bytes[i + 1].to_ascii_uppercase());
				out.push(bytes[i + 2].to_ascii_uppercase());
			}
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Normalise a host for matching as git's `url_normalize` does: escapes of unreserved octets are
/// decoded (`exam%70le.com` → `example.com`), the result is lower-cased, and **one** trailing FQDN dot
/// is dropped (`Example.COM.` matches `example.com`, but `example.com..` does not — git strips a single
/// dot). git normalises only for matching — the raw host still reaches a helper's `host=` line.
fn normalize_host(host: &str) -> String {
	let decoded = decode_unreserved(host);
	let single_dot_trimmed = decoded.strip_suffix('.').unwrap_or(&decoded);
	single_dot_trimmed.to_ascii_lowercase()
}

/// The port to compare after git's normalisation: leading zeros are stripped (git's rule 5, so `:0443`
/// and `:443` are the same port) and a port equal to the scheme's default is dropped (rule 6, so
/// `https://host` and `https://host:443` compare equal). A non-numeric port is compared verbatim.
fn normalize_port(scheme: &str, port: Option<String>) -> Option<String> {
	let port = port?;
	let canonical = match port.parse::<u32>() {
		Ok(number) => number.to_string(),
		// Not a number git would canonicalise — compare as given.
		Err(_) => port,
	};
	let default = match scheme {
		"https" => Some("443"),
		"http" => Some("80"),
		"ftps" => Some("990"),
		"ftp" => Some("21"),
		"ssh" => Some("22"),
		"git" => Some("9418"),
		_ => None,
	};
	(default != Some(canonical.as_str())).then_some(canonical)
}

/// git's `match_host`: compare host label by label (split on `.`); a whole `*` pattern label matches
/// any single host label, others must match exactly, and both sides must run out of labels together.
fn host_matches(host: &str, pattern: &str) -> bool {
	let mut host = host.split('.');
	let mut pattern = pattern.split('.');
	loop {
		match (host.next(), pattern.next()) {
			(Some(_), Some("*")) => {}
			(Some(h), Some(p)) if h == p => {}
			(None, None) => return true,
			_ => return false,
		}
	}
}

/// The request path in the leading-`/` form full-URL matching compares against a [`FullPattern::path`]
/// (`"/"` when there is no path). git derives it by *decoding* the request URL's path
/// (`credential_from_url`) and then *re-encoding* it (`credential_format`) before `url_normalize`, which
/// gives a stricter form than a pattern: an encoded slash collapses to a real separator (`a%2Fb` → `a/b`,
/// so it matches a pattern `a/b` not `a%2Fb`), a reserved `:`/`@` is re-escaped, a preserved `%00`
/// becomes `%2500`, and a raw `0xFF` byte becomes `%FF`.
fn normalize_request_path(path: Option<&str>) -> String {
	match path {
		Some(path) => format!(
			"/{}",
			percent_encode_request_path(&gitana_remote::percent_decode_bytes(path))
		),
		None => "/".to_owned(),
	}
}

/// Normalise a URL **pattern** path for full-URL matching as git's `url_normalize`
/// (`append_normalized_escapes`) does, so equivalent spellings compare equal: an escape of an
/// *unreserved* octet is decoded (`%41` → `A`); any other escape is kept with upper-cased hex (a
/// reserved `%2F`/`%3A` or a `%20` stays encoded); a *literal* reserved delimiter (`:`/`@`/`/`/`[`) is
/// kept; and a literal byte outside the safe set (a space, control byte, `%`, or UTF-8 octet) is
/// encoded. Deliberately looser on literal reserved chars than the request encoding above — matching
/// git's asymmetry, where a literal-`:` pattern does not match a request (whose `:` is re-escaped).
fn normalize_url_path(path: &str) -> String {
	const HEX: &[u8; 16] = b"0123456789ABCDEF";
	let bytes = path.as_bytes();
	let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& i + 2 < bytes.len()
			&& let (Some(hi), Some(lo)) = (
				(bytes[i + 1] as char).to_digit(16),
				(bytes[i + 2] as char).to_digit(16),
			) {
			let byte = (hi * 16 + lo) as u8;
			if is_unreserved(byte) {
				out.push(byte);
			} else {
				out.push(b'%');
				out.push(bytes[i + 1].to_ascii_uppercase());
				out.push(bytes[i + 2].to_ascii_uppercase());
			}
			i += 3;
		} else {
			let byte = bytes[i];
			if is_reserved_or_unreserved(byte) {
				out.push(byte);
			} else {
				// A literal byte outside the safe set (a space, control byte, `%`, or UTF-8 octet) is
				// encoded, as git canonicalises the URL before matching.
				out.push(b'%');
				out.push(HEX[(byte >> 4) as usize]);
				out.push(HEX[(byte & 0xf) as usize]);
			}
			i += 1;
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// git's `url_match_prefix`: `prefix` matches `url` when it is an exact match or a prefix ending on a
/// path-component boundary. Both are in leading-`/` form; an empty or `"/"` prefix matches any path.
fn path_prefix_matches(url: &str, prefix: &str) -> bool {
	if prefix.is_empty() || prefix == "/" {
		return url.is_empty() || url.starts_with('/');
	}
	let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
	url.starts_with(prefix) && (url.len() == prefix.len() || url.as_bytes()[prefix.len()] == b'/')
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A request for `https://<host>/<path>` with an optional known username.
	fn request(host: &str, path: Option<&str>, username: Option<&str>) -> CredentialRequest {
		CredentialRequest {
			protocol: "https".to_owned(),
			host: host.to_owned(),
			path: path.map(str::to_owned),
			username: username.map(str::to_owned),
			carried_username: None,
			wwwauth: Vec::new(),
			state: Vec::new(),
			authtype: None,
			ephemeral: false,
			caps_authtype: false,
			caps_state: false,
		}
	}

	#[test]
	fn full_url_scheme_host_and_path_prefix() {
		let req = request("example.com", Some("acme/app.git"), None);
		// Scheme + host, with and without a scheme; path prefixes at a boundary.
		assert!(subsection_matches("https://example.com", &req));
		assert!(subsection_matches("https://example.com/acme", &req));
		assert!(subsection_matches("https://example.com/acme/app.git", &req));
		// Wrong scheme, wrong host, and a non-boundary path prefix do not match.
		assert!(!subsection_matches("http://example.com", &req));
		assert!(!subsection_matches("https://other.com", &req));
		assert!(!subsection_matches("https://example.com/ac", &req));
	}

	#[test]
	fn scheme_less_pattern_matches_any_protocol_but_exact_host_and_path() {
		let req = request("example.com", Some("acme/app.git"), None);
		// A bare host matches any protocol (git's partial match, protocol unconstrained).
		assert!(subsection_matches("example.com", &req));
		// A partial path must match exactly, not as a prefix.
		assert!(subsection_matches("example.com/acme/app.git", &req));
		assert!(!subsection_matches("example.com/acme", &req));
		assert!(!subsection_matches("other.com", &req));
	}

	#[test]
	fn wildcard_host_matches_exactly_one_label() {
		let star = "https://*.example.com";
		assert!(subsection_matches(
			star,
			&request("sub.example.com", None, None)
		));
		// Zero labels and two labels do not match a single-`*`.
		assert!(!subsection_matches(
			star,
			&request("example.com", None, None)
		));
		assert!(!subsection_matches(
			star,
			&request("a.b.example.com", None, None)
		));
	}

	#[test]
	fn port_matches_with_default_stripping() {
		// A pattern with no port matches a request carrying the scheme's default port, and vice versa.
		assert!(subsection_matches(
			"https://example.com",
			&request("example.com:443", None, None)
		));
		assert!(subsection_matches(
			"https://example.com:443",
			&request("example.com", None, None)
		));
		assert!(subsection_matches(
			"https://example.com:8080",
			&request("example.com:8080", None, None)
		));
		// A non-default port must match.
		assert!(!subsection_matches(
			"https://example.com:8080",
			&request("example.com", None, None)
		));
	}

	#[test]
	fn port_comparison_ignores_leading_zeros() {
		// git canonicalises numeric ports: `:0443` is the default `443`, and `:08080` == `:8080`.
		assert!(subsection_matches(
			"https://example.com:0443",
			&request("example.com", None, None)
		));
		assert!(subsection_matches(
			"https://example.com:08080",
			&request("example.com:8080", None, None)
		));
	}

	#[test]
	fn host_escapes_and_malformed_ipv6_are_normalized() {
		// An escaped unreserved host octet decodes, so `exam%70le.com` matches `example.com`.
		assert!(subsection_matches(
			"https://exam%70le.com",
			&request("example.com", Some("repo"), None)
		));
		// A bracketed IPv6 host matches with or without its default port.
		assert!(subsection_matches(
			"https://[::1]",
			&request("[::1]", Some("repo"), None)
		));
		// A malformed suffix after the bracket (`[::1]evil`) must not match the canonical `[::1]`.
		assert!(!subsection_matches(
			"https://[::1]evil",
			&request("[::1]", Some("repo"), None)
		));
		// Only one trailing FQDN dot is stripped, so a doubly-dotted host does not match.
		assert!(subsection_matches(
			"https://example.com.",
			&request("example.com", Some("repo"), None)
		));
		assert!(!subsection_matches(
			"https://example.com..",
			&request("example.com", Some("repo"), None)
		));
	}

	#[test]
	fn a_malformed_or_nul_escape_pattern_falls_back_to_exact_matching() {
		// A malformed `%zz` fails full-URL normalization, so it matches only an *exact* request path
		// (partial fallback), never as a prefix.
		assert!(subsection_matches(
			"https://example.com/a%zz",
			&request("example.com", Some("a%zz"), None)
		));
		assert!(!subsection_matches(
			"https://example.com/a%zz",
			&request("example.com", Some("a%zz/repo"), None)
		));
		// An encoded NUL never matches an `a%00b` pattern (rejected as non-normalizable)...
		assert!(!subsection_matches(
			"https://example.com/a%00b",
			&request("example.com", Some("a%00b"), None)
		));
		// ...but git re-encodes the request's preserved `%00` to `%2500`, so it matches an `a%2500b`
		// pattern (which is itself normalizable).
		assert!(subsection_matches(
			"https://example.com/a%2500b",
			&request("example.com", Some("a%00b"), None)
		));
	}

	#[test]
	fn request_authority_is_normalized_before_matching() {
		let pattern = "https://example.com";
		// git normalises the request authority before matching a canonical pattern: an uppercase scheme,
		// a trailing FQDN dot, an empty port, and the default port all still match.
		let mut req = request("example.com", Some("repo"), None);
		req.protocol = "HTTPS".to_owned();
		assert!(subsection_matches(pattern, &req));
		// An uppercase scheme still recognises its explicit default port (443) as the default.
		let mut req_443 = request("example.com:443", Some("repo"), None);
		req_443.protocol = "HTTPS".to_owned();
		assert!(subsection_matches(pattern, &req_443));
		assert!(subsection_matches(
			pattern,
			&request("example.com.", Some("repo"), None)
		));
		assert!(subsection_matches(
			pattern,
			&request("example.com:", Some("repo"), None)
		));
		assert!(subsection_matches(
			pattern,
			&request("example.com:443", Some("repo"), None)
		));
		// A trailing dot on the pattern normalises symmetrically.
		assert!(subsection_matches(
			"https://example.com.",
			&request("example.com", Some("repo"), None)
		));
	}

	#[test]
	fn path_matching_preserves_reserved_escapes() {
		// Full-URL matching normalises like git's `url_normalize`: an encoded space or slash stays
		// encoded, so both sides must spell the path the same way. The request path is percent-encoded.
		let req = request("example.com", Some("my%20project/app.git"), None);
		assert!(subsection_matches("https://example.com/my%20project", &req));
		// git canonicalises a literal space in the pattern to `%20`, so it matches the `%20` request too.
		assert!(subsection_matches("https://example.com/my project", &req));
		assert!(!subsection_matches("https://example.com/my", &req));

		// An encoded slash in a *pattern* is not a separator: `a%2Fb` must not match an ordinary `a/b` repo.
		let slashed = request("example.com", Some("a/b"), None);
		assert!(!subsection_matches("https://example.com/a%2Fb", &slashed));
		assert!(subsection_matches("https://example.com/a/b", &slashed));
		// But an encoded slash in the *request* collapses to a separator (git decodes the request path),
		// so `a%2Fb` matches the decoded pattern `a/b`, not the encoded `a%2Fb`.
		let enc_slash = request("example.com", Some("a%2Fb/repo"), None);
		assert!(subsection_matches("https://example.com/a/b", &enc_slash));
		assert!(!subsection_matches("https://example.com/a%2Fb", &enc_slash));
		// An unreserved escape (`%41` → `A`) is decoded on both sides, so it matches its decoded form.
		let encoded_a = request("example.com", Some("a%41b"), None);
		assert!(subsection_matches("https://example.com/aAb", &encoded_a));
	}

	#[test]
	fn userinfo_in_pattern_must_match_request_username() {
		let with_user = "https://alice@example.com";
		assert!(subsection_matches(
			with_user,
			&request("example.com", None, Some("alice"))
		));
		assert!(!subsection_matches(
			with_user,
			&request("example.com", None, Some("bob"))
		));
		// No known username on the request cannot satisfy a pattern that demands one.
		assert!(!subsection_matches(
			with_user,
			&request("example.com", None, None)
		));
	}

	#[test]
	fn a_query_or_fragment_pattern_matches_nothing_ordinary() {
		let req = request("example.com", Some("repo"), None);
		// git rejects a query/fragment in a credential.<url> pattern; it must not degrade to a
		// host-only match that would apply to every repo on the host.
		assert!(!subsection_matches("https://example.com?tenant=x", &req));
		assert!(!subsection_matches("https://example.com/repo?x", &req));
		assert!(!subsection_matches("https://example.com#frag", &req));
		// The same host without a query does match.
		assert!(subsection_matches("https://example.com", &req));
	}

	#[test]
	fn pattern_userinfo_is_percent_decoded_before_matching() {
		// The request username arrives already decoded (`alice@org`); a pattern spelt with `%40` must
		// decode to the same value to match.
		let req = request("example.com", None, Some("alice@org"));
		assert!(subsection_matches("https://alice%40org@example.com", &req));
		assert!(!subsection_matches(
			"https://alice%40other@example.com",
			&req
		));
	}

	#[test]
	fn an_empty_user_pattern_constrains_rather_than_broadens() {
		// `https://:secret@host` has present-but-empty userinfo: git treats it as a username constraint
		// that matches no ordinary request, not a host-wide pattern.
		assert!(!subsection_matches(
			"https://:secret@example.com",
			&request("example.com", Some("repo"), None)
		));
		assert!(!subsection_matches(
			"https://:secret@example.com",
			&request("example.com", None, Some("alice"))
		));
		// The scheme-less partial form behaves the same.
		assert!(!subsection_matches(
			":secret@example.com",
			&request("example.com", None, Some("alice"))
		));
	}

	#[test]
	fn resolve_accumulates_helpers_with_empty_reset_and_last_wins_singles() {
		let config = GitConfig::parse(concat!(
			"[credential]\n",
			"\thelper = store\n",
			"\tusername = generic\n",
			"[credential \"https://example.com\"]\n",
			"\thelper = \n",
			"\thelper = osxkeychain\n",
			"\tusername = specific\n",
			"\tuseHttpPath = true\n",
		))
		.unwrap();
		let resolved = resolve(&config, &request("example.com", Some("acme/app.git"), None)).unwrap();
		// The empty reset cleared `store`, leaving only the URL-matched `osxkeychain`.
		assert_eq!(resolved.helpers.len(), 1);
		// The per-URL username (later, matching) wins over the section-level one.
		assert_eq!(resolved.username.as_deref(), Some("specific"));
		assert!(resolved.use_http_path);
	}

	#[test]
	fn resolve_skips_non_matching_url_entries() {
		let config = GitConfig::parse(concat!(
			"[credential \"https://other.com\"]\n",
			"\thelper = osxkeychain\n",
			"\tusername = other\n",
		))
		.unwrap();
		let resolved = resolve(&config, &request("example.com", None, None)).unwrap();
		assert!(resolved.helpers.is_empty());
		assert_eq!(resolved.username, None);
		assert!(!resolved.use_http_path);
	}

	#[test]
	fn resolve_rejects_a_valueless_helper() {
		let config = GitConfig::parse("[credential]\n\thelper\n").unwrap();
		assert!(resolve(&config, &request("example.com", None, None)).is_err());
	}
}
