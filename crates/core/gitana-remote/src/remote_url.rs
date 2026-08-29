//! Scheme dispatch for a remote URL — the entry point that decides HTTP vs SSH.

use anyhow::{Result, bail};

use crate::{Origin, SshRemote, anonymize_url, percent_decode_bytes};

/// The URL scheme prefixes git routes through the SSH transport: the canonical `ssh://` and its
/// `git+ssh://` / `ssh+git://` aliases. The text after the prefix is the same `[user@]host[:port]/path`.
const SSH_SCHEMES: [&str; 3] = ["ssh://", "git+ssh://", "ssh+git://"];

/// A parsed remote URL, dispatched by scheme. The HTTP arm carries an [`Origin`], the SSH arm an
/// [`SshRemote`], and the local arm a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteUrl {
	/// A Smart HTTP(S) remote.
	Http(Origin),
	/// An SSH remote (`ssh://…` or the scp-like `[user@]host:path`).
	Ssh(SshRemote),
	/// A local filesystem repository (`file://…` or a bare path).
	Local(String),
}

impl RemoteUrl {
	/// The Git transport-policy scheme for this URL.
	pub fn scheme(&self) -> &'static str {
		match self {
			Self::Http(origin) if origin.url.starts_with("https://") => "https",
			Self::Http(_) => "http",
			Self::Ssh(_) => "ssh",
			Self::Local(_) => "file",
		}
	}

	/// Parse a remote URL, choosing the transport from its scheme:
	/// - `http://` / `https://` → [`RemoteUrl::Http`] (via [`Origin::parse`]);
	/// - `ssh://` / `git+ssh://` / `ssh+git://` `[user@]host[:port]/path` → [`RemoteUrl::Ssh`];
	/// - a scp-like `[user@]host:path` (no scheme, a `:` before any `/`) → [`RemoteUrl::Ssh`].
	///
	/// A `file://` URL or a non-scp-like string with no scheme is a local path. Other explicit schemes
	/// remain unsupported and are rejected with userinfo removed from the diagnostic.
	pub fn parse(url: &str) -> Result<Self> {
		if url.starts_with("http://") || url.starts_with("https://") {
			return Ok(Self::Http(Origin::parse(url)?));
		}
		for scheme in SSH_SCHEMES {
			if let Some(rest) = url.strip_prefix(scheme) {
				return Ok(Self::Ssh(SshRemote::parse_url(rest)?));
			}
		}
		// Test scp syntax before the file scheme: `file:/srv/repo` is the scp-like host `file`
		// with path `/srv/repo`, while only a spelling containing `file://` is a file URL.
		if SshRemote::is_scp_like(url) {
			return Ok(Self::Ssh(SshRemote::parse_scp(url)?));
		}
		if let Some(rest) = url.strip_prefix("file:") {
			return Ok(Self::Local(parse_file_url(url, rest)?));
		}
		if url.contains("://") {
			bail!("unsupported remote URL scheme: {}", anonymize_url(url));
		}
		if url.is_empty() {
			bail!("no path specified for local remote");
		}
		Ok(Self::Local(url.to_owned()))
	}
}

fn parse_file_url(url: &str, rest: &str) -> Result<String> {
	let path = if let Some(authority_and_path) = rest.strip_prefix("//") {
		if authority_and_path.starts_with('/') {
			authority_and_path
		} else {
			let Some((authority, path)) = authority_and_path.split_once('/') else {
				bail!("unsupported file URL authority: {}", anonymize_url(url));
			};
			if !authority.eq_ignore_ascii_case("localhost") {
				bail!("unsupported file URL authority: {}", anonymize_url(url));
			}
			return decoded_local_path(&format!("/{path}"));
		}
	} else if rest.starts_with('/') {
		rest
	} else {
		bail!(
			"file URL must contain an absolute path: {}",
			anonymize_url(url)
		);
	};
	decoded_local_path(path)
}

fn decoded_local_path(path: &str) -> Result<String> {
	if path.is_empty() {
		bail!("no path specified for local remote");
	}
	if path.to_ascii_lowercase().contains("%00") {
		bail!("local remote path contains an encoded NUL byte");
	}
	let path = String::from_utf8(percent_decode_bytes(path))
		.map_err(|_| anyhow::anyhow!("local remote path is not valid UTF-8"))?;
	Ok(strip_windows_url_slash(path))
}

fn strip_windows_url_slash(path: String) -> String {
	if !cfg!(windows) {
		return path;
	}
	let bytes = path.as_bytes();
	if bytes.len() >= 3
		&& bytes[0] == b'/'
		&& bytes[1].is_ascii_alphabetic()
		&& bytes[2] == b':'
		&& (bytes.len() == 3 || matches!(bytes[3], b'/' | b'\\'))
	{
		path[1..].to_owned()
	} else {
		path
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dispatches_http_to_origin() {
		let RemoteUrl::Http(origin) = RemoteUrl::parse("https://example.com/acme/app.git").unwrap()
		else {
			panic!("expected http");
		};
		assert_eq!(origin.url, "https://example.com/acme/app.git");
	}

	#[test]
	fn dispatches_ssh_url() {
		let RemoteUrl::Ssh(ssh) = RemoteUrl::parse("ssh://git@example.com:22/repo.git").unwrap() else {
			panic!("expected ssh");
		};
		assert_eq!(ssh.host, "example.com");
		assert_eq!(ssh.port, Some(22));
		assert_eq!(ssh.path, "/repo.git");
	}

	#[test]
	fn dispatches_scp_alias() {
		let RemoteUrl::Ssh(ssh) = RemoteUrl::parse("git@example.com:org/repo.git").unwrap() else {
			panic!("expected ssh");
		};
		assert_eq!(ssh.user.as_deref(), Some("git"));
		assert_eq!(ssh.path, "org/repo.git");

		let RemoteUrl::Ssh(ssh) = RemoteUrl::parse("file:/srv/repo").unwrap() else {
			panic!("file:/srv/repo is the scp-like host named file");
		};
		assert_eq!(ssh.host, "file");
		assert_eq!(ssh.path, "/srv/repo");
		assert_eq!(RemoteUrl::Ssh(ssh).scheme(), "ssh");
	}

	#[test]
	fn dispatches_ssh_scheme_aliases() {
		// git routes `git+ssh://` and `ssh+git://` through the SSH transport, same as `ssh://`.
		for url in ["git+ssh://git@host/repo.git", "ssh+git://git@host/repo.git"] {
			let RemoteUrl::Ssh(ssh) = RemoteUrl::parse(url).unwrap() else {
				panic!("expected ssh for {url}");
			};
			assert_eq!(ssh.host, "host");
			assert_eq!(ssh.path, "/repo.git");
		}
	}

	#[test]
	fn rejects_unsupported_scheme() {
		assert!(RemoteUrl::parse("git://example.com/repo.git").is_err());
	}

	#[test]
	fn dispatches_local_urls_and_paths() {
		assert_eq!(
			RemoteUrl::parse("/local/path").unwrap(),
			RemoteUrl::Local("/local/path".to_owned())
		);
		assert_eq!(
			RemoteUrl::parse("file:///local/a%20b").unwrap(),
			RemoteUrl::Local("/local/a b".to_owned())
		);
		assert_eq!(
			RemoteUrl::parse("file://localhost/local/repo").unwrap(),
			RemoteUrl::Local("/local/repo".to_owned())
		);
		for authority in ["LOCALHOST", "LocalHost"] {
			assert_eq!(
				RemoteUrl::parse(&format!("file://{authority}/local/repo")).unwrap(),
				RemoteUrl::Local("/local/repo".to_owned())
			);
		}
		assert_eq!(
			RemoteUrl::parse("../literal%20path").unwrap(),
			RemoteUrl::Local("../literal%20path".to_owned())
		);
		assert!(RemoteUrl::parse("").is_err());
		assert!(RemoteUrl::parse("file://").is_err());
		assert!(RemoteUrl::parse("file://host/repo").is_err());
		let credentialed = RemoteUrl::parse("file://user:secret@localhost/repo").unwrap_err();
		assert!(!credentialed.to_string().contains("secret"));
		assert!(RemoteUrl::parse("file:///local/%00repo").is_err());
	}

	#[cfg(windows)]
	#[test]
	fn dispatches_verbatim_windows_paths_as_local() {
		for path in [r"\\?\C:\repo", r"\\?\UNC\server\share\repo"] {
			assert_eq!(
				RemoteUrl::parse(path).unwrap(),
				RemoteUrl::Local(path.to_owned())
			);
		}
	}

	#[test]
	fn unsupported_scheme_error_hides_credentials() {
		let err = RemoteUrl::parse("git+foo://alice:secret@example.com/repo.git").unwrap_err();
		assert!(
			!format!("{err}").contains("secret"),
			"credential leaked in error: {err}"
		);
	}
}
