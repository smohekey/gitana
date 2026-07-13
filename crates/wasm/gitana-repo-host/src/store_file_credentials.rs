//! A file-backed credential source — the harness's default provider.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::HostCredentialProvider;
use crate::gitana::repo::credentials::{Credential, CredentialRequest, Filled};

/// A [`HostCredentialProvider`] backed by a file of `protocol://user:password@host` lines, in the shape
/// of git's `credential-store` — the harness's working default, proving a genuine `fill` → `approve`
/// (persist) → re-`fill` round-trip rather than a hardcoded value. The userinfo is percent-encoded on
/// write and decoded on read, so a `:`/`@`/`/` in a username or password round-trips.
///
/// It is a **test fixture, not a production credential store**, and deliberately does *not* reimplement
/// git's helper in full (credential machinery is the embedder's job, per this initiative): matching is
/// host-scoped and exact — the authority after `@` is an opaque key, so a git `useHttpPath` line like
/// `…@host/repo.git` is preserved verbatim but only answers a request for that exact key, not a
/// host-only one — and it assumes a single writer (the harness), with no cross-process lock. The store
/// is plaintext, as git's is, and is (re)written owner-only (mode `0600` on Unix) so the secret is not
/// exposed to other local users.
pub struct StoreFileCredentials {
	path: PathBuf,
}

impl StoreFileCredentials {
	/// A source over the store file at `path`. The file need not exist yet — a missing file reads as an
	/// empty store, and [`approve`](HostCredentialProvider::approve) creates it (mode `0600`).
	pub fn new(path: impl Into<PathBuf>) -> Self {
		Self { path: path.into() }
	}

	/// The stored entries, newest first (missing or unreadable file → empty).
	fn entries(&self) -> Vec<Entry> {
		fs::read_to_string(&self.path)
			.unwrap_or_default()
			.lines()
			.filter_map(Entry::parse)
			.collect()
	}

	/// Rewrite the store file with `entries`, one URL line each, owner-readable only.
	fn write(&self, entries: &[Entry]) {
		let body: String = entries
			.iter()
			.map(|entry| format!("{}\n", entry.line()))
			.collect();
		let _ = write_owner_only(&self.path, body.as_bytes());
	}
}

/// The Basic username/password of `cred`, or `None` for a pre-encoded (Bearer/…) credential this
/// plaintext store cannot represent — such a credential is simply not persisted here.
fn basic_parts(cred: &Credential) -> Option<(&str, &str)> {
	// The plaintext store keys a Basic username/password pair; an encoded credential (a pre-encoded
	// `authtype`/`credential`, e.g. a Bearer token) has no plaintext pair to persist here, so skip it.
	match (&cred.username, &cred.password, &cred.credential) {
		(Some(username), Some(password), None) => Some((username, password)),
		_ => None,
	}
}

impl HostCredentialProvider for StoreFileCredentials {
	fn fill(&self, request: &CredentialRequest) -> Option<Filled> {
		self
			.entries()
			.into_iter()
			.find(|entry| entry.matches(request))
			.map(|entry| Filled {
				// The store is Basic-only, and not multistage — a single non-ephemeral credential.
				credential: Credential {
					username: Some(entry.username),
					password: Some(entry.password),
					authtype: None,
					credential: None,
					ephemeral: false,
				},
				state: Vec::new(),
				more: false,
				caps_authtype: false,
				caps_state: false,
			})
	}

	fn approve(&self, request: &CredentialRequest, cred: &Credential) {
		let Some((username, password)) = basic_parts(cred) else {
			return;
		};
		let mut entries = self.entries();
		// De-dupe and prepend, as git's store does: drop any prior entry with the same key and username,
		// then insert the accepted one first so it wins the next `fill`. Other entries — including
		// path-scoped ones for unrelated hosts — are carried through untouched.
		entries.retain(|entry| !entry.keyed_as(request, username));
		entries.insert(0, Entry::for_request(request, username, password));
		self.write(&entries);
	}

	fn reject(&self, request: &CredentialRequest, cred: &Credential) {
		let Some((username, _)) = basic_parts(cred) else {
			return;
		};
		let mut entries = self.entries();
		entries.retain(|entry| !entry.keyed_as(request, username));
		self.write(&entries);
	}
}

/// Write `bytes` to `path`, truncating, with owner-only permissions (mode `0600` on Unix — a no-op
/// elsewhere) so the plaintext secret store is not world-readable, as git's `store` helper ensures.
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let mut options = fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut file = options.open(path)?;
	// `mode` above only applies when the file is created; force `0600` so a pre-existing file is
	// tightened too (matching git, which always keeps the store owner-only).
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		file.set_permissions(fs::Permissions::from_mode(0o600))?;
	}
	file.write_all(bytes)
}

/// One parsed store line — a credential keyed by protocol and an opaque authority key.
struct Entry {
	protocol: String,
	/// The authority after `@`, kept verbatim: usually `host` or `host:port`, but a git `useHttpPath`
	/// line carries `host/path`. Matched exactly and serialized back unchanged, so no entry is altered.
	key: String,
	username: String,
	password: String,
}

impl Entry {
	/// The entry `approve` records for `request` — keyed on its `protocol`/`host` (this provider writes
	/// host-scoped keys, never a path).
	fn for_request(request: &CredentialRequest, username: &str, password: &str) -> Self {
		Self {
			protocol: request.protocol.clone(),
			key: request.host.clone(),
			username: username.to_owned(),
			password: password.to_owned(),
		}
	}

	/// Parse a `protocol://user:password@authority` line — the userinfo percent-decoded, the authority
	/// kept opaque. `None` for a blank or malformed line.
	fn parse(line: &str) -> Option<Self> {
		let line = line.trim();
		if line.is_empty() {
			return None;
		}
		let (protocol, rest) = line.split_once("://")?;
		// The userinfo runs to the first `@`; a literal `@` inside it is percent-encoded (`%40`), so the
		// remainder is the authority key, taken verbatim.
		let (userinfo, key) = rest.split_once('@')?;
		let (username, password) = userinfo.split_once(':')?;
		Some(Self {
			protocol: protocol.to_owned(),
			key: key.to_owned(),
			username: percent_decode(username),
			password: percent_decode(password),
		})
	}

	/// The `protocol://user:password@authority` line this entry serialises to, the userinfo
	/// percent-encoded and the authority key reproduced verbatim.
	fn line(&self) -> String {
		format!(
			"{}://{}:{}@{}",
			self.protocol,
			percent_encode(&self.username),
			percent_encode(&self.password),
			self.key
		)
	}

	/// Whether this entry answers `request`: same protocol, the authority key equal to the request host
	/// (exact, host-scoped), and — when the request names a username — the same username.
	fn matches(&self, request: &CredentialRequest) -> bool {
		self.protocol == request.protocol
			&& self.key == request.host
			&& request
				.username
				.as_ref()
				.is_none_or(|username| username == &self.username)
	}

	/// Whether this entry is keyed on `request`'s protocol/host and the exact `username` — the identity
	/// `approve`/`reject` replace or erase on.
	fn keyed_as(&self, request: &CredentialRequest, username: &str) -> bool {
		self.protocol == request.protocol && self.key == request.host && self.username == username
	}
}

/// Percent-decode `input` (`%XX` → byte), leaving a lone or malformed `%` literal. Lossy UTF-8, as a
/// store line is text.
fn percent_decode(input: &str) -> String {
	let bytes = input.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& i + 2 < bytes.len()
			&& let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
		{
			out.push((hi << 4) | lo);
			i += 3;
			continue;
		}
		out.push(bytes[i]);
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode `input`, leaving only the URL-unreserved octets (`A-Za-z0-9-._~`) literal. More
/// aggressive than git's own encoder, but every result decodes identically and git reads it fine.
fn percent_encode(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	for &byte in input.as_bytes() {
		if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
			out.push(byte as char);
		} else {
			out.push_str(&format!("%{byte:02X}"));
		}
	}
	out
}

/// The value of a single hex digit, or `None` if `byte` is not one.
fn hex_value(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request(host: &str) -> CredentialRequest {
		CredentialRequest {
			protocol: "https".to_owned(),
			host: host.to_owned(),
			path: None,
			username: None,
			carried_username: None,
			wwwauth: Vec::new(),
			state: Vec::new(),
			authtype: None,
			ephemeral: false,
			caps_authtype: false,
			caps_state: false,
		}
	}

	fn credential(username: &str, password: &str) -> Credential {
		Credential {
			username: Some(username.to_owned()),
			password: Some(password.to_owned()),
			authtype: None,
			credential: None,
			ephemeral: false,
		}
	}

	/// The Basic username a `fill` resolved (the store is Basic-only).
	fn filled_username(filled: &Filled) -> &str {
		filled
			.credential
			.username
			.as_deref()
			.expect("store-file fill yields a Basic username")
	}

	#[test]
	fn userinfo_percent_round_trips() {
		let entry = Entry {
			protocol: "https".to_owned(),
			key: "example.com".to_owned(),
			username: "ali@ce".to_owned(),
			password: "p:s/w%d".to_owned(),
		};
		let parsed = Entry::parse(&entry.line()).expect("round-trips");
		assert_eq!(parsed.username, "ali@ce");
		assert_eq!(parsed.password, "p:s/w%d");
		assert_eq!(parsed.key, "example.com");
	}

	#[test]
	fn matches_host_with_port_exactly() {
		let entry = Entry::parse("https://u:p@localhost:8080").expect("parses");
		assert!(entry.matches(&request("localhost:8080")));
		assert!(!entry.matches(&request("localhost")));
	}

	#[test]
	fn path_scoped_line_is_preserved_verbatim() {
		// A git `useHttpPath` entry: kept byte-for-byte, and matched only for its exact key.
		let line = "https://ali%40ce:secret@example.com/acme.git";
		let parsed = Entry::parse(line).expect("parses");
		assert_eq!(parsed.username, "ali@ce");
		assert_eq!(parsed.key, "example.com/acme.git");
		assert_eq!(parsed.line(), line);
		assert!(!parsed.matches(&request("example.com")));
	}

	#[test]
	fn approve_leaves_unrelated_entries_untouched() {
		// A path-scoped entry for one host must survive an approve/rewrite for another, byte-for-byte.
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("store");
		let untouched = "https://u:p@other.example/a.git";
		fs::write(&path, format!("{untouched}\n")).unwrap();

		StoreFileCredentials::new(&path).approve(&request("example.com"), &credential("u", "new"));

		let lines: Vec<String> = fs::read_to_string(&path)
			.unwrap()
			.lines()
			.map(str::to_owned)
			.collect();
		assert!(
			lines.iter().any(|line| line == untouched),
			"unrelated path-scoped entry was altered: {lines:?}"
		);
	}

	#[test]
	fn approve_prepends_so_newest_wins() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("store");
		let store = StoreFileCredentials::new(&path);
		let req = request("example.com");
		store.approve(&req, &credential("old", "1"));
		store.approve(&req, &credential("new", "2"));
		// No username hint → the most recently approved (prepended) entry answers.
		assert_eq!(filled_username(&store.fill(&req).expect("filled")), "new");
		// Both persist (distinct usernames are not de-duped).
		assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
	}

	#[cfg(unix)]
	#[test]
	fn store_file_is_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("store");
		StoreFileCredentials::new(&path).approve(&request("host"), &credential("u", "p"));
		let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600, "store must be owner-only, got {mode:o}");
	}
}
