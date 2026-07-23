//! git's `http.extraHeader` — the extra HTTP request headers configured for a remote, resolved with
//! git's URL-match *specificity* (`urlmatch.c`).
//!
//! git attaches `http.extraHeader` values to every request. A value may be set section-wide
//! (`http.extraHeader`) or for a URL (`http.<url>.extraHeader`); among the entries whose `<url>`
//! matches the request, only those at the **single most-specific matching level** apply — a matching
//! `http.<url>` replaces the section-level (and any less-specific `http.<url>`), it does not add to it.
//! Entries at that one level accumulate in config order, and an empty value (`http.…extraHeader =`)
//! resets the accumulated list. Specificity ranks by matched host *literal* length, then path length,
//! then whether the pattern pinned a username — git's `cmp_matches`; two equally-specific patterns both
//! apply. Scheme and host compare case-insensitively, the (leading-zero-canonicalised) default port
//! (443/80) is stripped, a `*` wildcards a single host label in any position, and a trailing slash in a
//! pattern path counts toward specificity but is ignored for the match.
//!
//! This intentionally does not reimplement git's full `url_normalize` (percent-escape canonicalisation)
//! or match a `http.<user@host>` pattern (the request URL here carries no userinfo); those are noted
//! simplifications for uncommon inputs. It shares no code with the credential URL matcher
//! (`gitana-credential`) yet — the two are candidates for a future shared `urlmatch` module.

use anyhow::{Result, anyhow};
use gitana_config::GitConfig;

/// The `http.extraHeader` values that apply to a request to `url`, as `(name, value)` header pairs, in
/// the order git would send them. Empty when `url` is unparseable or nothing matches. A valueless
/// `http.extraHeader` (no `=`) is a config error git aborts on, so it is an error here too — otherwise a
/// token set by an earlier entry could be sent under a malformed config git would have rejected.
pub fn extra_headers(config: &GitConfig, url: &str) -> Result<Vec<(String, String)>> {
	let Some(target) = Url::parse(url) else {
		return Ok(Vec::new());
	};

	// Walk the entries in config order, keeping only those at the most-specific matching level seen so
	// far; a strictly-more-specific entry resets the collection, an equally-specific one adds to it.
	let mut best: Option<Specificity> = None;
	let mut collected: Vec<&str> = Vec::new();
	for (subsection, value) in config.variables_named("http", "extraheader") {
		// git's urlmatch filters by the `<url>` subsection *before* reading the value, so a valueless
		// entry only aborts when it actually applies — a non-matching `http.<url>.extraHeader` is skipped.
		let specificity = match subsection {
			None => Specificity::SECTION,
			Some(pattern) => match Url::parse(pattern).and_then(|p| p.match_specificity(&target)) {
				Some(specificity) => specificity,
				None => continue,
			},
		};
		// A valueless matching entry (no `=`) is a config error git aborts on (`config_error_nonbool`);
		// error here too rather than leaving an earlier token collected under a config git would reject.
		let value = value.ok_or_else(|| {
			let scope = subsection.map_or("http.extraheader".to_owned(), |url| {
				format!("http.{url}.extraheader")
			});
			anyhow!("missing value for '{scope}'")
		})?;

		match best {
			Some(current) if specificity < current => continue,
			Some(current) if specificity == current => {}
			_ => {
				best = Some(specificity);
				collected.clear();
			}
		}
		// Within the winning level, values accumulate in order and an empty value resets the list.
		if value.is_empty() {
			collected.clear();
		} else {
			collected.push(value);
		}
	}

	Ok(collected.into_iter().filter_map(split_header).collect())
}

/// Split an `X-Name: value` header line into its name and value, dropping a single space after the
/// colon (as git/curl do). `None` for a line with no colon (malformed) or with an empty value:
/// curl reads `Name:` as *removing* that header, and gta cannot un-generate one, so it simply does not
/// send the (empty) header — the full curl removal of an internally-generated header is not reproduced.
fn split_header(raw: &str) -> Option<(String, String)> {
	let (name, value) = raw.split_once(':')?;
	let value = value.strip_prefix(' ').unwrap_or(value);
	if value.is_empty() {
		return None;
	}
	Some((name.trim().to_owned(), value.to_owned()))
}

/// A parsed URL — the request target, or an `http.<url>` pattern (whose `host` may carry a `*.` label
/// wildcard). Scheme and host are lower-cased and the scheme's default port is stripped, so comparisons
/// are direct.
struct Url {
	scheme: String,
	user: Option<String>,
	host: String,
	port: Option<String>,
	/// The path with a leading `/` (`/` when the URL had none), including any query/fragment suffix and
	/// trailing slash (git matches those exactly).
	path: String,
}

impl Url {
	/// Parse `raw` as `scheme://[user[:pass]@]host[:port][/path]`; `None` when it has no scheme or host.
	fn parse(raw: &str) -> Option<Self> {
		let (scheme, rest) = raw.split_once("://")?;
		if scheme.is_empty() {
			return None;
		}
		let scheme = scheme.to_ascii_lowercase();
		let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
		let (user, host_port) = match rest[..authority_end].rsplit_once('@') {
			// Only the username participates in matching; a present-but-empty userinfo pins an empty user.
			Some((userinfo, host_port)) => {
				let user = userinfo.split(':').next().unwrap_or("");
				(Some(gitana_remote::percent_decode(user)), host_port)
			}
			None => (None, &rest[..authority_end]),
		};
		let (host, port) = split_host_port(host_port);
		if host.is_empty() {
			return None;
		}
		// The path keeps its query and fragment: git treats them as exact matching constraints, so a
		// `…/repo?tenant=a` pattern matches only a request with that same suffix, not a bare `…/repo`. The
		// trailing slash is kept too — it counts toward specificity (git ranks `…/repo/` above `…/repo`)
		// while [`path_matches`] trims it for the boundary comparison so it still matches a slashless target.
		let path = &rest[authority_end..];
		let path = if path.is_empty() {
			"/".to_owned()
		} else if path.starts_with('/') {
			path.to_owned()
		} else {
			format!("/{path}")
		};
		// A single trailing dot on an FQDN is insignificant (`example.com.` == `example.com`), so git
		// strips it before comparing; do the same so both spellings match.
		let host = host.to_ascii_lowercase();
		let host = host.strip_suffix('.').unwrap_or(&host);
		Some(Self {
			port: normalize_port(&scheme, port),
			scheme,
			user,
			host: host.to_owned(),
			path,
		})
	}

	/// The match specificity of `self` (an `http.<url>` pattern) against `target` (the request URL), or
	/// `None` if it does not match. Scheme and port must be equal, a pinned username must match, the host
	/// must match (exact or `*.` wildcard), and the pattern path must be a boundary prefix of the target.
	fn match_specificity(&self, target: &Url) -> Option<Specificity> {
		if self.scheme != target.scheme || self.port != target.port {
			return None;
		}
		if let Some(user) = &self.user
			&& target.user.as_deref() != Some(user.as_str())
		{
			return None;
		}
		let host_len = host_match(&self.host, &target.host)?;
		if !path_matches(&self.path, &target.path) {
			return None;
		}
		Some(Specificity {
			host_len,
			path_len: self.path.len(),
			user_matched: self.user.is_some(),
		})
	}
}

/// The URL-match specificity, ranked (as git's `cmp_matches`) by matched host length, then matched path
/// length, then whether a username was pinned. Field order *is* the comparison order (`derive(Ord)`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Specificity {
	host_len: usize,
	path_len: usize,
	user_matched: bool,
}

impl Specificity {
	/// The specificity of a section-level `http.extraHeader` (no `<url>`): it matches every request, at
	/// the lowest possible specificity, so any matching `http.<url>` outranks it.
	const SECTION: Self = Self {
		host_len: 0,
		path_len: 0,
		user_matched: false,
	};
}

/// Match `target_host` against a pattern host label-by-label, returning the total matched *literal*
/// length (for specificity) or `None`. Each `*` label wildcards a single host label (in any position —
/// `*.example.com`, `api.*.example.com`); a literal label must match exactly, and the label counts must
/// be equal (so `*.example.com` matches `a.example.com` but not `a.b.example.com`). git ranks host
/// specificity by total literal length: an exact host outranks a wildcarded one, and a pattern with more
/// literal characters outranks one with fewer (`longprefix.*.example.com` beats `*.x.example.com`); two
/// patterns with equal literal length are equally specific, so their headers both apply.
fn host_match(pattern: &str, target_host: &str) -> Option<usize> {
	let pattern_labels: Vec<&str> = pattern.split('.').collect();
	let target_labels: Vec<&str> = target_host.split('.').collect();
	if pattern_labels.len() != target_labels.len() {
		return None;
	}
	let mut literal_len = 0;
	for (pattern_label, target_label) in pattern_labels.iter().zip(&target_labels) {
		if *pattern_label == "*" {
			// A wildcard label matches any single non-empty label but contributes no literal specificity.
			if target_label.is_empty() {
				return None;
			}
		} else if pattern_label != target_label {
			return None;
		} else {
			literal_len += pattern_label.len();
		}
	}
	Some(literal_len)
}

/// Whether `pattern_path` is a boundary prefix of `target_path` (both leading-`/`): the match must end
/// at a `/` boundary or the end of the target, so `/acme` matches `/acme` and `/acme/app` but not
/// `/acme-x`. git ignores a *single* trailing slash on the pattern (so `/repo/` matches `/repo`, but
/// `/repo//` matches only `/repo/`); the target is not trimmed. A pattern of `/` matches every path.
fn path_matches(pattern_path: &str, target_path: &str) -> bool {
	let pattern = pattern_path.strip_suffix('/').unwrap_or(pattern_path);
	match target_path.strip_prefix(pattern) {
		Some(rest) => rest.is_empty() || rest.starts_with('/'),
		None => false,
	}
}

/// Split `host[:port]` into its host and optional port, handling a `[…]`-bracketed IPv6 literal. A
/// non-numeric suffix after a `:` is treated as part of the host (not a port).
fn split_host_port(host_port: &str) -> (String, Option<String>) {
	if let Some(rest) = host_port.strip_prefix('[')
		&& let Some((addr, after)) = rest.split_once(']')
	{
		// An empty port (`[::1]:`) normalises to no port, as git does.
		let port = after
			.strip_prefix(':')
			.filter(|port| !port.is_empty())
			.map(str::to_owned);
		return (format!("[{addr}]"), port);
	}
	match host_port.rsplit_once(':') {
		// An empty port (`host:`) normalises to no port (git treats it as the default port).
		Some((host, "")) => (host.to_owned(), None),
		Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => {
			(host.to_owned(), Some(port.to_owned()))
		}
		_ => (host_port.to_owned(), None),
	}
}

/// Canonicalise the port (stripping leading zeros, so `0443` and `443` compare equal, as git does) and
/// drop the scheme's default port (443 for `https`, 80 for `http`) so `host` and `host:443` compare
/// equal.
fn normalize_port(scheme: &str, port: Option<String>) -> Option<String> {
	let default = match scheme {
		"https" => "443",
		"http" => "80",
		_ => "",
	};
	port
		.map(|port| canonical_port(&port))
		.filter(|port| port != default)
}

/// A numeric port with leading zeros stripped (an all-zero port canonicalises to `0`).
fn canonical_port(port: &str) -> String {
	match port.trim_start_matches('0') {
		"" => "0".to_owned(),
		trimmed => trimmed.to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(text: &str) -> GitConfig {
		GitConfig::parse(text).expect("parse config")
	}

	fn names(headers: &[(String, String)]) -> Vec<&str> {
		headers.iter().map(|(name, _)| name.as_str()).collect()
	}

	#[test]
	fn most_specific_level_wins_and_replaces() {
		let config = parse(
			"[http]\n\textraHeader = X-Base: base\n\
			 [http \"https://example.com\"]\n\textraHeader = X-Host: host\n\
			 [http \"https://example.com/acme/app.git\"]\n\textraHeader = X-Path: path\n",
		);
		// The path-level entry outranks host- and section-level, and replaces them.
		assert_eq!(
			extra_headers(&config, "https://example.com/acme/app.git").unwrap(),
			vec![("X-Path".to_owned(), "path".to_owned())]
		);
		// A URL under the host but off the path falls back to the host-level entry.
		assert_eq!(
			names(&extra_headers(&config, "https://example.com/other").unwrap()),
			vec!["X-Host"]
		);
		// A different host gets only the section-level entry.
		assert_eq!(
			names(&extra_headers(&config, "https://elsewhere.test/x").unwrap()),
			vec!["X-Base"]
		);
	}

	#[test]
	fn same_level_entries_accumulate_and_empty_resets() {
		let config =
			parse("[http \"https://example.com\"]\n\textraHeader = X-One: 1\n\textraHeader = X-Two: 2\n");
		assert_eq!(
			names(&extra_headers(&config, "https://example.com/x").unwrap()),
			vec!["X-One", "X-Two"]
		);
		// An empty value resets the accumulated list.
		let reset = parse("[http]\n\textraHeader = X-A: a\n\textraHeader =\n\textraHeader = X-B: b\n");
		assert_eq!(
			names(&extra_headers(&reset, "https://host/x").unwrap()),
			vec!["X-B"]
		);
	}

	#[test]
	fn exact_host_outranks_wildcard_even_with_shorter_path() {
		let config = parse(
			"[http \"https://*.example.com/very/deep/path\"]\n\textraHeader = X-WildDeep: wd\n\
			 [http \"https://foo.example.com\"]\n\textraHeader = X-ExactShallow: es\n",
		);
		assert_eq!(
			names(&extra_headers(&config, "https://foo.example.com/very/deep/path/x").unwrap()),
			vec!["X-ExactShallow"]
		);
		// The wildcard still matches a sibling host the exact pattern does not.
		assert_eq!(
			names(&extra_headers(&config, "https://bar.example.com/very/deep/path").unwrap()),
			vec!["X-WildDeep"]
		);
	}

	#[test]
	fn wildcard_matches_single_label_only() {
		let config = parse("[http \"https://*.example.com\"]\n\textraHeader = X-W: w\n");
		assert_eq!(
			names(&extra_headers(&config, "https://a.example.com/x").unwrap()),
			vec!["X-W"]
		);
		// Two labels before the suffix do not match a single-label wildcard.
		assert!(
			extra_headers(&config, "https://a.b.example.com/x")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn wildcard_matches_a_middle_label() {
		// git accepts a `*` label anywhere in the host, not only as the leading label.
		let config = parse("[http \"https://api.*.example.com\"]\n\textraHeader = X-Mid: m\n");
		assert_eq!(
			names(&extra_headers(&config, "https://api.foo.example.com/x").unwrap()),
			vec!["X-Mid"]
		);
		assert!(
			extra_headers(&config, "https://api.example.com/x")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn trailing_slash_path_is_more_specific() {
		// git ranks `…/repo/` above `…/repo`, so only the slash-terminated header applies to `/repo/x`.
		let config = parse(
			"[http \"https://host/repo\"]\n\textraHeader = X-NoSlash: n\n\
			 [http \"https://host/repo/\"]\n\textraHeader = X-Slash: s\n",
		);
		assert_eq!(
			names(&extra_headers(&config, "https://host/repo/x").unwrap()),
			vec!["X-Slash"]
		);
	}

	#[test]
	fn longer_literal_host_outranks_shorter() {
		// git ranks host by total literal length, so `longprefix.*` (more literal) beats `*.x`.
		let config = parse(
			"[http \"https://longprefix.*.example.com\"]\n\textraHeader = X-Long: l\n\
			 [http \"https://*.x.example.com\"]\n\textraHeader = X-Short: s\n",
		);
		assert_eq!(
			names(&extra_headers(&config, "https://longprefix.x.example.com/y").unwrap()),
			vec!["X-Long"]
		);
	}

	#[test]
	fn equal_literal_length_wildcards_both_apply() {
		// `api.*` and `*.foo` have equal literal length → equally specific, so both headers apply, in
		// config order (git accumulates a tie; `--get-urlmatch` shows only the last, but the transport
		// sends both).
		let config = parse(
			"[http \"https://api.*.example.com\"]\n\textraHeader = X-Api: a\n\
			 [http \"https://*.foo.example.com\"]\n\textraHeader = X-Foo: f\n",
		);
		assert_eq!(
			names(&extra_headers(&config, "https://api.foo.example.com/x").unwrap()),
			vec!["X-Api", "X-Foo"]
		);
	}

	#[test]
	fn leading_zero_port_matches() {
		// git canonicalises numeric ports, so a `:0443` pattern matches a `:443` (default) request.
		let config = parse("[http \"https://host:0443\"]\n\textraHeader = X-Z: z\n");
		assert_eq!(
			names(&extra_headers(&config, "https://host:443/x").unwrap()),
			vec!["X-Z"]
		);
		assert_eq!(
			names(&extra_headers(&config, "https://host/x").unwrap()),
			vec!["X-Z"]
		);
	}

	#[test]
	fn trailing_slash_in_pattern_path_still_matches() {
		// git matches a `…/repo/` pattern against a `…/repo` (and `…/repo/x`) request.
		let config = parse("[http \"https://host/repo/\"]\n\textraHeader = X-Slash: s\n");
		assert_eq!(
			names(&extra_headers(&config, "https://host/repo").unwrap()),
			vec!["X-Slash"]
		);
		assert_eq!(
			names(&extra_headers(&config, "https://host/repo/x").unwrap()),
			vec!["X-Slash"]
		);
		assert!(
			extra_headers(&config, "https://host/repository")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn default_port_is_stripped_and_scheme_case_insensitive() {
		let config = parse("[http \"https://example.com\"]\n\textraHeader = X-P: p\n");
		assert_eq!(
			names(&extra_headers(&config, "https://example.com:443/x").unwrap()),
			vec!["X-P"]
		);
		assert_eq!(
			names(&extra_headers(&config, "HTTPS://EXAMPLE.COM/x").unwrap()),
			vec!["X-P"]
		);
		// A non-default port does not match a port-less pattern.
		assert!(
			extra_headers(&config, "https://example.com:8443/x")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn header_value_splits_on_first_colon() {
		let config = parse("[http]\n\textraHeader = Authorization: Bearer a:b:c\n");
		assert_eq!(
			extra_headers(&config, "https://host/x").unwrap(),
			vec![("Authorization".to_owned(), "Bearer a:b:c".to_owned())]
		);
	}

	#[test]
	fn query_and_fragment_are_exact_constraints() {
		// git treats a query/fragment as an exact match constraint, not a broadenable prefix.
		let config = parse("[http \"https://host/repo?tenant=a\"]\n\textraHeader = X-Q: q\n");
		assert!(
			extra_headers(&config, "https://host/repo")
				.unwrap()
				.is_empty()
		);
		assert_eq!(
			names(&extra_headers(&config, "https://host/repo?tenant=a").unwrap()),
			vec!["X-Q"]
		);
		assert!(
			extra_headers(&config, "https://host/repo?tenant=b")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn valueless_extra_header_is_an_error() {
		// A bare, *matching* `extraHeader` (no `=`) is a config error git aborts on.
		let config = parse("[http]\n\textraHeader\n");
		assert!(extra_headers(&config, "https://host/x").is_err());
	}

	#[test]
	fn non_matching_valueless_extra_header_is_skipped() {
		// git's urlmatch filters before reading the value, so a valueless `http.<url>` that does not
		// match the request is skipped rather than aborting.
		let config = parse("[http \"https://other.example\"]\n\textraHeader\n");
		assert!(extra_headers(&config, "https://host/x").unwrap().is_empty());
	}

	#[test]
	fn trailing_host_dot_is_ignored() {
		let config = parse("[http \"https://example.com/repo\"]\n\textraHeader = X-Dot: d\n");
		assert_eq!(
			names(&extra_headers(&config, "https://example.com./repo").unwrap()),
			vec!["X-Dot"]
		);
	}

	#[test]
	fn only_one_trailing_slash_is_ignored() {
		// git ignores a single trailing slash, so `/repo//` matches `/repo/` but not `/repo`.
		let config = parse("[http \"https://host/repo//\"]\n\textraHeader = X-DS: 1\n");
		assert_eq!(
			names(&extra_headers(&config, "https://host/repo/").unwrap()),
			vec!["X-DS"]
		);
		assert!(
			extra_headers(&config, "https://host/repo")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn empty_port_normalises_to_none() {
		// `host:` (empty port) is equivalent to the default port, so it matches a port-less pattern.
		let config = parse("[http \"https://host/repo\"]\n\textraHeader = X-EP: 1\n");
		assert_eq!(
			names(&extra_headers(&config, "https://host:/repo").unwrap()),
			vec!["X-EP"]
		);
	}

	#[test]
	fn empty_value_header_is_not_sent() {
		// curl reads `Name:` (empty value) as removal; gta at least does not send an empty header.
		let config = parse("[http]\n\textraHeader = X-Foo:\n");
		assert!(extra_headers(&config, "https://host/x").unwrap().is_empty());
	}

	#[test]
	fn no_config_yields_no_headers() {
		let config = parse("[core]\n\tbare = false\n");
		assert!(extra_headers(&config, "https://host/x").unwrap().is_empty());
	}
}
