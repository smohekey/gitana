//! Parsing an SSH remote URL — the `ssh://` and scp-like `[user@]host:path` forms.

use anyhow::{Result, bail};

use crate::percent_decode;

/// An SSH Git remote: an optional login user, a host, an optional port, and the repository path the
/// remote `git-upload-pack` / `git-receive-pack` is invoked on. Parsed from either the URL form
/// `ssh://[user@]host[:port]/path` or the scp-like alias `[user@]host:path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshRemote {
	/// The login user (`ssh`'s `[user@]host`), if the URL named one.
	pub user: Option<String>,
	/// The host to connect to (bare host or bracket-stripped IPv6 literal).
	pub host: String,
	/// The port, only from the `ssh://host:port` form — the scp-like alias cannot carry a port, matching
	/// git.
	pub port: Option<u16>,
	/// The repository path as sent to the remote, verbatim. `ssh://` paths are absolute (leading `/`
	/// kept, except a `/~` git strips so the remote shell expands `~`); scp-like paths are relative as
	/// written.
	pub path: String,
}

impl SshRemote {
	/// Parse the `ssh://[user@]host[:port]/path` URL form (the `rest` is the text after `ssh://`).
	pub fn parse_url(rest: &str) -> Result<Self> {
		let (authority, path) = match rest.find('/') {
			Some(slash) => (&rest[..slash], &rest[slash..]),
			None => (rest, ""),
		};
		if authority.is_empty() {
			bail!("ssh URL has no host");
		}
		// git rejects an `ssh://host[:port]` with no path ("no path specified") rather than running
		// `git-upload-pack ''` on the remote login directory. (The scp alias `host:` sends `''`, which git
		// allows — so this check is `ssh://`-only.) A root path `ssh://host/` is a path and is kept.
		if path.is_empty() {
			bail!("no path specified in ssh URL");
		}
		let (user, host_port) = split_user(authority);
		let (host, port) = split_host_port(host_port)?;
		if host.is_empty() {
			bail!("ssh URL has no host");
		}
		// git percent-decodes the components of an `ssh://` URL (unlike the scp-like alias, which it
		// passes literally) — so `%20`→space, `%40`→`@` — before invoking ssh. Decode, then validate the
		// decoded values, so an encoded `-` (`%2D…`) is still caught by the option-injection guard.
		let user = user.map(|user| percent_decode(&user));
		let host = percent_decode(host);
		let path = percent_decode(path);
		// git strips a single leading slash before `~` so the remote shell performs tilde expansion
		// (`ssh://host/~user/repo.git` → the remote command `git-upload-pack '~user/repo.git'`).
		let path = match path.strip_prefix("/~") {
			Some(tail) => format!("~{tail}"),
			None => path,
		};
		reject_option_injection(user.as_deref(), &host, &path)?;
		Ok(Self {
			user,
			host,
			port,
			path,
		})
	}

	/// Whether `url` is the scp-like `[user@]host:path` alias: no URL scheme, and a `:` that comes
	/// before any `/` (and is not inside a leading `[…]` IPv6 bracket).
	pub fn is_scp_like(url: &str) -> bool {
		scp_separator(url).is_some()
	}

	/// Parse the scp-like `[user@]host:path` alias. The separating `:` is the first one that is not
	/// inside a `[…]` IPv6 bracket; the path is taken verbatim (relative), and no port is possible.
	pub fn parse_scp(url: &str) -> Result<Self> {
		let colon = scp_separator(url).ok_or_else(|| anyhow::anyhow!("not an scp-like remote"))?;
		let (authority, path) = (&url[..colon], &url[colon + 1..]);
		let (user, host_port) = split_user(authority);
		// The scp alias has no port; a bracketed IPv6 literal keeps its brackets stripped.
		let host = host_port
			.strip_prefix('[')
			.and_then(|inner| inner.strip_suffix(']'))
			.unwrap_or(host_port);
		if host.is_empty() {
			bail!("scp-like remote has no host");
		}
		reject_option_injection(user.as_deref(), host, path)?;
		Ok(Self {
			user,
			host: host.to_owned(),
			port: None,
			path: path.to_owned(),
		})
	}
}

/// Split an `[user@]host…` authority on the last `@` into `(user, host…)`. A `user:password` userinfo
/// keeps only the user (the ssh transport carries no password), and an empty user is dropped.
fn split_user(authority: &str) -> (Option<String>, &str) {
	match authority.rsplit_once('@') {
		Some((userinfo, host)) => {
			let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
			let user = (!user.is_empty()).then(|| user.to_owned());
			(user, host)
		}
		None => (None, authority),
	}
}

/// Split a `host[:port]` (or bracketed `[ipv6][:port]`) into `(host, port)`.
fn split_host_port(host_port: &str) -> Result<(&str, Option<u16>)> {
	if let Some(rest) = host_port.strip_prefix('[') {
		// Bracketed IPv6 literal: `[addr]` or `[addr]:port`.
		let close = rest
			.find(']')
			.ok_or_else(|| anyhow::anyhow!("ssh URL has an unterminated IPv6 bracket"))?;
		let host = &rest[..close];
		let after = &rest[close + 1..];
		let port = match after.strip_prefix(':') {
			Some(port) => parse_port(port)?,
			None if after.is_empty() => None,
			None => bail!("unexpected text after IPv6 host: {after}"),
		};
		return Ok((host, port));
	}
	match host_port.rsplit_once(':') {
		Some((host, port)) => Ok((host, parse_port(port)?)),
		None => Ok((host_port, None)),
	}
}

/// Reject a `[user@]host` or path that `ssh` (or the remote `git-upload-pack`) would mistake for a
/// command-line option — one beginning with `-`. This mirrors git's `looks_like_command_line_option`
/// guard (the CVE-2017-1000117 fix): a URL like `ssh://-oProxyCommand=payload/repo` must not let the
/// destination string reach `ssh` as an option. Only a **leading** `-` on the whole `[user@]host`
/// argument is dangerous — `git@-h` is safe (ssh parses `-h` as the host field of a `user@host` token,
/// not an option), matching git, which blocks a bare `-h` host but not `git@-h`.
fn reject_option_injection(user: Option<&str>, host: &str, path: &str) -> Result<()> {
	// The ssh destination argument is `user@host` when a user is present, else the bare host; its first
	// character is the user's (present ⇒ non-empty) or the host's.
	if user.unwrap_or(host).starts_with('-') {
		let destination = match user {
			Some(user) => format!("{user}@{host}"),
			None => host.to_owned(),
		};
		bail!("strange hostname '{destination}' blocked (looks like a command-line option)");
	}
	if path.starts_with('-') {
		bail!("strange pathname '{path}' blocked (looks like a command-line option)");
	}
	Ok(())
}

/// Parse an `ssh://` URL port, or `None` when the component is empty — git treats an explicit empty
/// port (`ssh://host:/path`) as unspecified and uses the default. Percent-decodes first (git decodes
/// the component), so `ssh://host:%32%32/…` (`%32%32` → `22`) is a valid `-p 22`, not a parse error.
fn parse_port(port: &str) -> Result<Option<u16>> {
	let decoded = percent_decode(port);
	if decoded.is_empty() {
		return Ok(None);
	}
	decoded
		.parse::<u16>()
		.map(Some)
		.map_err(|_| anyhow::anyhow!("invalid ssh port: {decoded}"))
}

/// The index of the scp separator `:` in `url`, or `None` if `url` is not scp-like. The separator is the
/// first `:` that comes before any `/` and is not inside a leading `[…]` IPv6 bracket. On Windows a
/// leading DOS drive prefix (`C:\repo` / `C:/repo`) is a local path, not an scp host, so it is excluded
/// there (git's `has_dos_drive_prefix`); on other platforms `C:repo` is a valid scp remote (host `C`).
fn scp_separator(url: &str) -> Option<usize> {
	if url.contains("://") {
		return None;
	}
	if has_dos_drive_prefix(url) {
		return None;
	}
	// A bracketed IPv6 host may sit at the start or right after a `user@` (git's `@[`); the host/path
	// separator is the first `:` after the closing `]`.
	let bracket = if url.starts_with('[') {
		Some(0)
	} else {
		url.find("@[").map(|at| at + 1)
	};
	if let Some(open) = bracket {
		let close = url[open..].find(']')? + open;
		let after = &url[close + 1..];
		return after.starts_with(':').then_some(close + 1);
	}
	let colon = url.find(':')?;
	match url.find('/') {
		Some(slash) if slash < colon => None,
		_ => Some(colon),
	}
}

/// Whether `url` begins with a Windows DOS drive prefix (`<letter>:`). Only meaningful on Windows —
/// there `C:\repo` is a local path, not the scp remote `C:repo` — so it is always `false` elsewhere.
#[cfg(windows)]
fn has_dos_drive_prefix(url: &str) -> bool {
	let bytes = url.as_bytes();
	bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(not(windows))]
fn has_dos_drive_prefix(_url: &str) -> bool {
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_ssh_url_with_user_and_port() {
		// Matches git's probed command: `ssh -p 2222 git@example.com git-upload-pack '/path/to/repo.git'`.
		let ssh = SshRemote::parse_url("git@example.com:2222/path/to/repo.git").unwrap();
		assert_eq!(ssh.user.as_deref(), Some("git"));
		assert_eq!(ssh.host, "example.com");
		assert_eq!(ssh.port, Some(2222));
		assert_eq!(ssh.path, "/path/to/repo.git");
	}

	#[test]
	fn parses_ssh_url_without_user_or_port() {
		let ssh = SshRemote::parse_url("example.com/srv/repo.git").unwrap();
		assert_eq!(ssh.user, None);
		assert_eq!(ssh.host, "example.com");
		assert_eq!(ssh.port, None);
		assert_eq!(ssh.path, "/srv/repo.git");
	}

	#[test]
	fn strips_leading_slash_before_tilde() {
		// `ssh://example.com/~user/rel.git` → remote command `git-upload-pack '~user/rel.git'`.
		let ssh = SshRemote::parse_url("example.com/~user/rel.git").unwrap();
		assert_eq!(ssh.path, "~user/rel.git");
	}

	#[test]
	fn parses_scp_alias_relative_path() {
		// `git@example.com:group/repo.git` → `git-upload-pack 'group/repo.git'` (relative).
		let ssh = SshRemote::parse_scp("git@example.com:group/repo.git").unwrap();
		assert_eq!(ssh.user.as_deref(), Some("git"));
		assert_eq!(ssh.host, "example.com");
		assert_eq!(ssh.port, None);
		assert_eq!(ssh.path, "group/repo.git");
	}

	#[test]
	fn scp_alias_takes_the_first_colon() {
		// The path may itself contain a colon; only the first (host) colon separates.
		let ssh = SshRemote::parse_scp("host:a:b/repo.git").unwrap();
		assert_eq!(ssh.host, "host");
		assert_eq!(ssh.path, "a:b/repo.git");
	}

	#[test]
	fn rejects_option_injection_hostname() {
		// A bare host beginning with `-` would reach `ssh` as an option (CVE-2017-1000117 class).
		assert!(SshRemote::parse_url("-oProxyCommand=payload/repo.git").is_err());
		assert!(SshRemote::parse_scp("-oProxyCommand=payload:repo.git").is_err());
		// A user beginning with `-` is equally dangerous (the whole `-u@host` argument leads with `-`).
		assert!(SshRemote::parse_url("-u@host/repo.git").is_err());
		// But `git@-h` is safe — ssh parses `-h` as the host field of the `user@host` token, not an
		// option — and git allows it, so we must too.
		assert!(SshRemote::parse_scp("git@-h:repo.git").is_ok());
		assert!(SshRemote::parse_url("git@-h/repo.git").is_ok());
	}

	#[test]
	fn rejects_option_injection_pathname() {
		// An scp path beginning with `-` would reach the remote `git-upload-pack` as an option.
		assert!(SshRemote::parse_scp("host:-oProxyCommand=payload").is_err());
		// The `ssh://` form keeps the leading `/`, so `/-foo` is a safe pathname, not an option.
		assert!(SshRemote::parse_url("host/-foo/repo.git").is_ok());
	}

	#[test]
	fn parses_bracketed_ipv6_host() {
		let url = SshRemote::parse_url("git@[::1]:2222/repo.git").unwrap();
		assert_eq!(url.host, "::1");
		assert_eq!(url.port, Some(2222));
		assert_eq!(url.path, "/repo.git");

		let scp = SshRemote::parse_scp("[::1]:repo.git").unwrap();
		assert_eq!(scp.host, "::1");
		assert_eq!(scp.path, "repo.git");
	}

	#[test]
	fn parses_user_prefixed_ipv6_scp() {
		// git resolves `git@[::1]:repo.git` to host `::1`, path `repo.git` (the separator is the `:` after
		// `]`, not the first colon inside the address).
		let scp = SshRemote::parse_scp("git@[::1]:repo.git").unwrap();
		assert_eq!(scp.user.as_deref(), Some("git"));
		assert_eq!(scp.host, "::1");
		assert_eq!(scp.path, "repo.git");
		assert_eq!(scp_separator("git@[::1]:repo.git"), Some(9));
	}

	#[test]
	fn ssh_url_empty_port_is_the_default() {
		// git treats an explicit empty port as unspecified (default port, no `-p`).
		assert_eq!(SshRemote::parse_url("host:/repo.git").unwrap().port, None);
		assert_eq!(
			SshRemote::parse_url("git@[::1]:/repo.git").unwrap().port,
			None
		);
	}

	#[test]
	fn ssh_url_percent_decodes_the_port() {
		// git decodes the port component too: `%32%32` → `22`.
		let ssh = SshRemote::parse_url("host:%32%32/repo.git").unwrap();
		assert_eq!(ssh.port, Some(22));
	}

	#[test]
	fn ssh_url_percent_decodes_components() {
		// git decodes `ssh://` components: `%40`→`@` in the user, `%20`→space in the path.
		let ssh = SshRemote::parse_url("user%40name@host/path%20repo.git").unwrap();
		assert_eq!(ssh.user.as_deref(), Some("user@name"));
		assert_eq!(ssh.host, "host");
		assert_eq!(ssh.path, "/path repo.git");
	}

	#[test]
	fn rejects_ssh_url_without_a_path() {
		// git rejects `ssh://host` / `ssh://host:port` ("no path specified") rather than serving the login
		// directory; a root path `/` is allowed.
		assert!(SshRemote::parse_url("host").is_err());
		assert!(SshRemote::parse_url("host:2222").is_err());
		assert!(SshRemote::parse_url("host/").is_ok());
	}

	#[test]
	fn scp_alias_does_not_decode() {
		// git passes an scp-like path literally (no percent-decoding), unlike an `ssh://` URL.
		let scp = SshRemote::parse_scp("git@host:path%20repo.git").unwrap();
		assert_eq!(scp.path, "path%20repo.git");
	}

	#[test]
	fn ssh_url_rejects_encoded_option_injection() {
		// An option-injection host smuggled through percent-encoding (`%2D` → `-`) is still blocked, since
		// the guard runs on the decoded value.
		assert!(SshRemote::parse_url("%2DoProxyCommand=payload/repo.git").is_err());
	}

	#[test]
	fn scp_separator_detects_the_shape() {
		// scp-like: a `:` before any `/`.
		assert_eq!(scp_separator("git@host:org/repo.git"), Some(8));
		// A URL with a scheme is never scp-like.
		assert_eq!(scp_separator("ssh://host/repo.git"), None);
		// A `:` only after a `/` is a path, not an scp host separator.
		assert_eq!(scp_separator("./dir:name"), None);
		// No colon at all.
		assert_eq!(scp_separator("/local/path"), None);
		// Bracketed IPv6 scp.
		assert_eq!(scp_separator("[::1]:repo.git"), Some(5));
	}

	#[test]
	#[cfg(not(windows))]
	fn drive_letter_is_scp_off_windows() {
		// Off Windows, `C:/repo` is the scp remote `C:/repo` (host `C`) — matching git, which only
		// treats a DOS drive prefix as a local path on Windows.
		assert_eq!(scp_separator("C:/repo"), Some(1));
		assert!(SshRemote::is_scp_like("C:/repo"));
	}

	#[test]
	#[cfg(windows)]
	fn drive_letter_is_a_local_path_on_windows() {
		// On Windows `C:\repo` / `C:/repo` is a local path, not the scp remote `C:repo`.
		assert_eq!(scp_separator("C:/repo"), None);
		assert_eq!(scp_separator("C:\\repo"), None);
		assert!(!SshRemote::is_scp_like("C:/repo"));
	}
}
