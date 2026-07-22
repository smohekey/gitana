//! git's `url.<base>.insteadOf` / `pushInsteadOf` URL rewriting, applied in the remote transport path.
//!
//! git rewrites a remote URL before use: the longest `url.<base>.insteadOf` whose value is a prefix of
//! the URL has that prefix replaced with `<base>` (ties resolve to the first rule in config order).
//! For a push, `pushInsteadOf` takes precedence when one of its prefixes matches, otherwise `insteadOf`
//! applies. This is the same rewriting `gta remote -v` reports; here it resolves the remote URL string
//! that `clone`/`fetch`/`pull`/`push` parse (`RemoteUrl` for transport dispatch) and talk to.

use anyhow::{Context, Result, anyhow, bail};
use gitana_config::GitConfig;

/// The surviving values of the multi-valued `remote.<remote>.<key>` (`url` or `pushurl`), in config
/// order with git's empty-value **reset** applied — an empty (`= ""`) value clears everything
/// accumulated so far, so a higher-scope `url =` wipes a lower-scope value. A *valueless* entry (a bare
/// `url` with no `=`) is a config error git aborts on, so it is an error here too — otherwise git's
/// `get_all` drops it and a stale lower-scope url would silently survive. Shared by the fetch/push
/// origin resolution and the `remote -v` listing. Uses `variables_named` (not `get_all`, which discards
/// the valueless marker) to see the raw entries in order.
pub(crate) fn remote_urls<'a>(
	config: &'a GitConfig,
	remote: &str,
	key: &str,
) -> Result<Vec<&'a str>> {
	let mut urls: Vec<&str> = Vec::new();
	for (subsection, value) in config.variables_named("remote", key) {
		if subsection != Some(remote) {
			continue;
		}
		match value {
			None => bail!("missing value for 'remote.{remote}.{key}'"),
			Some("") => urls.clear(),
			Some(url) => urls.push(url),
		}
	}
	Ok(urls)
}

/// Resolve the fetch-direction remote URL for `remote` from `config`: the **first** surviving
/// `remote.<remote>.url` (git fetches from the first) with `url.*.insteadOf` applied. The caller parses
/// it (`RemoteUrl` for the transport dispatch, or `Origin` where only HTTP is supported). Used by
/// `fetch`/`pull` (and `trust sync`) — `clone` rewrites its CLI argument directly with
/// [`rewrite_fetch_url`].
pub fn resolve_fetch_url(config: &GitConfig, remote: &str) -> Result<String> {
	let urls = remote_urls(config, remote, "url")?;
	let url = *urls
		.first()
		.with_context(|| format!("no remote.{remote}.url configured"))?;
	rewrite_fetch_url(config, url)
}

/// A URL rewriter: applies git's `insteadOf`/`pushInsteadOf` rules to one remote URL.
type UrlRewrite = fn(&GitConfig, &str) -> Result<String>;

/// A credential-safe form of `url` for display, persistence, or a push certificate's pushee: the URL
/// verbatim (scheme case, path, trailing slash preserved) with only a `:password` stripped from the
/// userinfo — the username is kept (git keeps it; it is not a secret), the password is not. A string
/// with no `://` (an scp-like alias, which carries no password) is returned unchanged. Used wherever a
/// remote URL leaves the authentication path, so a plaintext credential never reaches a print, a
/// `.git/config`, a reflog-adjacent commit message, or a signed certificate.
pub fn redact_password(url: &str) -> String {
	let Some((scheme, rest)) = url.split_once("://") else {
		return url.to_owned();
	};
	let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
	let (authority, tail) = rest.split_at(authority_end);
	// The userinfo is delimited by the last `@`; the username is up to the first `:` (so a password may
	// contain either). An empty username drops the whole `userinfo@`.
	let authority = match authority.rsplit_once('@') {
		Some((userinfo, host)) => {
			let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
			if user.is_empty() {
				host.to_owned()
			} else {
				format!("{user}@{host}")
			}
		}
		None => authority.to_owned(),
	};
	format!("{scheme}://{authority}{tail}")
}

/// Resolve the push-direction remote URL for `remote`, matching git's push-URL selection: the
/// `remote.<remote>.pushurl`s (with `insteadOf` rewriting) if any, else the `remote.<remote>.url`s with
/// `pushInsteadOf` (falling back to `insteadOf`). The caller parses it (`RemoteUrl` for dispatch).
///
/// git pushes to *every* surviving destination (mirroring); gta pushes to a single one, so more than one
/// is an explicit error rather than a silent push to just one — leaving the other mirrors stale would be
/// worse than declining. (Multi-destination push is a deferred feature.)
pub fn resolve_push_url(config: &GitConfig, remote: &str) -> Result<String> {
	let pushurls = remote_urls(config, remote, "pushurl")?;
	// A pushurl is rewritten with `insteadOf`; a fetch url falling through is rewritten with
	// `pushInsteadOf`. Both honour git's empty-value reset via `remote_urls`.
	let (destinations, rewrite): (Vec<&str>, UrlRewrite) = if pushurls.is_empty() {
		(remote_urls(config, remote, "url")?, rewrite_push_url)
	} else {
		(pushurls, rewrite_fetch_url)
	};
	match destinations.as_slice() {
		[] => bail!("no remote.{remote}.url configured"),
		[one] => rewrite(config, one),
		_ => bail!(
			"remote.{remote} has multiple push destinations; gta pushes to a single destination \
			 (multi-destination push is not yet supported)"
		),
	}
}

/// Rewrite `url` for a fetch-direction operation (`clone`/`fetch`/`pull`): apply `url.*.insteadOf`.
pub fn rewrite_fetch_url(config: &GitConfig, url: &str) -> Result<String> {
	Ok(rewrite(url, &rules(config, "insteadOf")?))
}

/// Rewrite `url` for a push: `url.*.pushInsteadOf` when one of its prefixes matches `url`, else fall
/// back to `insteadOf` — matching git (`remote.c`'s `alias_url` with the push rewrite set, then the
/// fetch set). Apply this to `remote.<name>.url`; an explicit `remote.<name>.pushurl` is rewritten with
/// [`rewrite_fetch_url`] instead (git rewrites a pushurl with plain `insteadOf`).
pub fn rewrite_push_url(config: &GitConfig, url: &str) -> Result<String> {
	let push = rules(config, "pushInsteadOf")?;
	if starts_with_rule(url, &push) {
		Ok(rewrite(url, &push))
	} else {
		rewrite_fetch_url(config, url)
	}
}

/// The URL-rewrite rules for `key` (`insteadOf` or `pushInsteadOf`): each `url.<base>.<key> = <prefix>`
/// as a `(prefix, base)` pair, in config file order so ties resolve to the first rule as git does. A
/// valueless entry (`url.<base>.insteadOf` with no `=`) is a config error git aborts on, so it is an
/// error here too; an entry with no `<base>` subsection is not a rewrite rule and is skipped.
fn rules<'a>(config: &'a GitConfig, key: &str) -> Result<Vec<(&'a str, &'a str)>> {
	config
		.variables_named("url", key)
		.into_iter()
		.filter_map(|(base, prefix)| {
			let base = base?;
			Some(match prefix {
				Some(prefix) => Ok((prefix, base)),
				None => Err(anyhow!("missing value for 'url.{base}.{key}'")),
			})
		})
		.collect()
}

/// Whether any rule's prefix is a prefix of `url`.
fn starts_with_rule(url: &str, rules: &[(&str, &str)]) -> bool {
	rules.iter().any(|(prefix, _)| url.starts_with(prefix))
}

/// Rewrite `url` by the longest matching rule prefix (replacing it with that rule's base), or return it
/// unchanged when nothing matches. On an equal-length tie the first rule in config order wins, as git
/// does.
fn rewrite(url: &str, rules: &[(&str, &str)]) -> String {
	let mut best: Option<&(&str, &str)> = None;
	for rule in rules.iter().filter(|(prefix, _)| url.starts_with(prefix)) {
		if best.is_none_or(|current| rule.0.len() > current.0.len()) {
			best = Some(rule);
		}
	}
	match best {
		Some((prefix, base)) => format!("{base}{}", &url[prefix.len()..]),
		None => url.to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(text: &str) -> GitConfig {
		GitConfig::parse(text).expect("parse config")
	}

	#[test]
	fn redact_password_preserves_spelling() {
		// Password dropped, username + trailing slash + scheme case preserved.
		assert_eq!(
			redact_password("HTTPS://alice:secret@host/repo/"),
			"HTTPS://alice@host/repo/"
		);
		// A password containing `@`/`:` is still fully removed (last `@`, first `:`).
		assert_eq!(
			redact_password("https://alice:se@cr:et@host/r"),
			"https://alice@host/r"
		);
		// No userinfo, and a userinfo-less scp-like alias, pass through unchanged (an ssh login user is
		// kept — it is not a secret).
		assert_eq!(redact_password("https://host/r"), "https://host/r");
		assert_eq!(
			redact_password("git@host:org/repo.git"),
			"git@host:org/repo.git"
		);
		assert_eq!(redact_password("ssh://git@host/r"), "ssh://git@host/r");
		// An empty username drops the whole userinfo.
		assert_eq!(redact_password("https://:secret@host/r"), "https://host/r");
	}

	#[test]
	fn fetch_applies_insteadof_longest_prefix() {
		let config = parse(
			"[url \"https://internal/\"]\n\tinsteadOf = https://example.com/\n\
			 [url \"https://deep-internal/\"]\n\tinsteadOf = https://example.com/acme/\n",
		);
		assert_eq!(
			rewrite_fetch_url(&config, "https://example.com/acme/app.git").unwrap(),
			"https://deep-internal/app.git"
		);
		assert_eq!(
			rewrite_fetch_url(&config, "https://example.com/other.git").unwrap(),
			"https://internal/other.git"
		);
	}

	#[test]
	fn valueless_remote_url_is_an_error() {
		// A bare `url` (no `=`) is a config error git aborts on — not a silent revival of a prior url.
		let config = parse("[remote \"origin\"]\n\turl = https://old.example/r\n\turl\n");
		assert!(resolve_fetch_url(&config, "origin").is_err());
	}

	#[test]
	fn valueless_insteadof_is_an_error() {
		// A bare `insteadOf` (no `=`) is a config error git aborts on.
		let config = parse("[url \"https://mirror/\"]\n\tinsteadOf\n");
		assert!(rewrite_fetch_url(&config, "https://example.com/app.git").is_err());
	}

	#[test]
	fn no_matching_rule_leaves_url_unchanged() {
		let config = parse("[url \"https://internal/\"]\n\tinsteadOf = git://old/\n");
		assert_eq!(
			rewrite_fetch_url(&config, "https://example.com/app.git").unwrap(),
			"https://example.com/app.git"
		);
	}

	#[test]
	fn push_prefers_pushinsteadof_then_falls_back_to_insteadof() {
		let config = parse(
			"[url \"https://push-target/\"]\n\tpushInsteadOf = https://example.com/\n\
			 [url \"https://fetch-target/\"]\n\tinsteadOf = https://example.com/\n",
		);
		// pushInsteadOf wins for a push.
		assert_eq!(
			rewrite_push_url(&config, "https://example.com/app.git").unwrap(),
			"https://push-target/app.git"
		);
		// With no pushInsteadOf match, push falls back to insteadOf.
		let only_fetch = parse("[url \"https://fetch-target/\"]\n\tinsteadOf = https://example.com/\n");
		assert_eq!(
			rewrite_push_url(&only_fetch, "https://example.com/app.git").unwrap(),
			"https://fetch-target/app.git"
		);
	}

	#[test]
	fn resolve_push_url_prefers_pushurl_then_url() {
		// An explicit pushurl (insteadOf-rewritten) wins over the fetch url.
		let with_pushurl = parse(
			"[remote \"origin\"]\n\turl = https://fetch.example/r\n\tpushurl = https://push.example/r\n",
		);
		assert_eq!(
			resolve_push_url(&with_pushurl, "origin").unwrap(),
			"https://push.example/r"
		);
		// With no pushurl, the fetch url is rewritten with pushInsteadOf.
		let no_pushurl = parse(
			"[remote \"origin\"]\n\turl = https://example.com/r\n\
			 [url \"https://mirror/\"]\n\tpushInsteadOf = https://example.com/\n",
		);
		assert_eq!(
			resolve_push_url(&no_pushurl, "origin").unwrap(),
			"https://mirror/r"
		);
	}

	#[test]
	fn resolve_push_url_rejects_multiple_pushurls() {
		let config = parse(
			"[remote \"origin\"]\n\turl = https://example.com/r\n\
			 \tpushurl = https://a.example/r\n\tpushurl = https://b.example/r\n",
		);
		assert!(resolve_push_url(&config, "origin").is_err());
	}

	#[test]
	fn fetch_uses_first_url_with_empty_reset() {
		// git fetches from the FIRST surviving url.
		let two = parse(
			"[remote \"origin\"]\n\turl = https://first.example/r\n\turl = https://second.example/r\n",
		);
		assert_eq!(
			resolve_fetch_url(&two, "origin").unwrap(),
			"https://first.example/r"
		);
		// An empty url resets the accumulated list, so a later url becomes the sole survivor.
		let reset = parse(
			"[remote \"origin\"]\n\turl = https://stale.example/r\n\turl =\n\turl = https://live.example/r\n",
		);
		assert_eq!(
			resolve_fetch_url(&reset, "origin").unwrap(),
			"https://live.example/r"
		);
	}

	#[test]
	fn push_rejects_multiple_urls_without_pushurl() {
		// git pushes to every url when no pushurl is set; gta is single-destination and declines.
		let config =
			parse("[remote \"origin\"]\n\turl = https://a.example/r\n\turl = https://b.example/r\n");
		assert!(resolve_push_url(&config, "origin").is_err());
	}

	#[test]
	fn empty_pushurl_resets_to_fetch_url() {
		// git treats an empty `pushurl =` as a reset: `A` then empty falls back to `url`, not to `A`.
		let config = parse(
			"[remote \"origin\"]\n\turl = https://example.com/r\n\
			 \tpushurl = https://a.example/r\n\tpushurl =\n",
		);
		assert_eq!(
			resolve_push_url(&config, "origin").unwrap(),
			"https://example.com/r"
		);
	}

	#[test]
	fn first_rule_wins_on_equal_length_tie() {
		let config = parse(
			"[url \"https://first/\"]\n\tinsteadOf = https://example.com/\n\
			 [url \"https://second/\"]\n\tinsteadOf = https://example.com/\n",
		);
		assert_eq!(
			rewrite_fetch_url(&config, "https://example.com/app.git").unwrap(),
			"https://first/app.git"
		);
	}
}
