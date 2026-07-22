//! Scheme dispatch for a remote URL — the entry point that decides HTTP vs SSH.

use anyhow::{Result, bail};

use crate::{Origin, SshRemote, anonymize_url};

/// The URL scheme prefixes git routes through the SSH transport: the canonical `ssh://` and its
/// `git+ssh://` / `ssh+git://` aliases. The text after the prefix is the same `[user@]host[:port]/path`.
const SSH_SCHEMES: [&str; 3] = ["ssh://", "git+ssh://", "ssh+git://"];

/// A parsed remote URL, dispatched by scheme. `clone` matches on this to pick a transport; the HTTP
/// arm carries the existing [`Origin`], the SSH arm an [`SshRemote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteUrl {
	/// A Smart HTTP(S) remote.
	Http(Origin),
	/// An SSH remote (`ssh://…` or the scp-like `[user@]host:path`).
	Ssh(SshRemote),
}

impl RemoteUrl {
	/// Parse a remote URL, choosing the transport from its scheme:
	/// - `http://` / `https://` → [`RemoteUrl::Http`] (via [`Origin::parse`]);
	/// - `ssh://` / `git+ssh://` / `ssh+git://` `[user@]host[:port]/path` → [`RemoteUrl::Ssh`];
	/// - a scp-like `[user@]host:path` (no scheme, a `:` before any `/`) → [`RemoteUrl::Ssh`].
	///
	/// Other schemes (`git://`, `file://`, a bare local path) are not yet supported and are rejected —
	/// with any userinfo anonymised out of the error, so a credential never reaches the logs.
	pub fn parse(url: &str) -> Result<Self> {
		let trimmed = url.trim_end_matches('/');
		if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
			return Ok(Self::Http(Origin::parse(url)?));
		}
		for scheme in SSH_SCHEMES {
			if let Some(rest) = url.strip_prefix(scheme) {
				return Ok(Self::Ssh(SshRemote::parse_url(rest)?));
			}
		}
		if SshRemote::is_scp_like(url) {
			return Ok(Self::Ssh(SshRemote::parse_scp(url)?));
		}
		bail!("unsupported remote URL scheme: {}", anonymize_url(url));
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
		assert!(RemoteUrl::parse("/local/path").is_err());
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
