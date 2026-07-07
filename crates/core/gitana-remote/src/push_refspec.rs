//! Parsing of Git *push* refspecs (`[+]<src>[:<dst>]`).
//!
//! A push refspec maps a **local** ref (`src`) to a **remote** ref to update (`dst`) — the inverse of
//! a fetch refspec, which maps an advertised remote ref to a local one. A leading `+` forces the
//! update (permits a non-fast-forward). An empty source (`:<dst>`) deletes the remote ref. With no
//! colon, `<name>` pushes to the same-name remote ref (git's DWIM). Wildcards are not supported yet.
//!
//! Qualification is **static** (parse-time), so an unqualified name resolves to a **branch** unless
//! its own refspec makes the namespace clear (`refs/tags/v1:v2` → `refs/tags/v2`). Pushing or deleting
//! a tag therefore needs its full `refs/tags/<name>` on the side that would otherwise default to a
//! branch — e.g. `:refs/tags/v1`, not `:v1`. Resolving a bare name against the *remote's advertised*
//! refs (so `:v1` finds an existing `refs/tags/v1`) is deferred to the tags work, which plans against
//! the advertisement.

use anyhow::{Result, bail};

/// A parsed push refspec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRefspec {
	/// A `+`-prefixed refspec forces the update (allows a non-fast-forward).
	pub force: bool,
	/// The local source ref (`refs/heads/main`, `HEAD`, `refs/tags/v1`, …). `None` for a deletion
	/// (`:<dst>`), which has no source.
	pub src: Option<String>,
	/// The remote destination ref to update or delete — fully qualified (`refs/heads/main`), or the
	/// literal `HEAD`, which the pusher resolves to the current branch's name at push time (git's
	/// `push origin HEAD` shorthand).
	pub dst: String,
}

impl PushRefspec {
	/// Parse a single push refspec: `[+]<src>:<dst>`, `[+]:<dst>` (delete), or `[+]<name>` (push to the
	/// same-name remote ref). Sources and destinations are qualified to full ref names (a bare `<name>`
	/// becomes `refs/heads/<name>`; `HEAD` and `refs/*` pass through).
	pub fn parse(text: &str) -> Result<Self> {
		let text = text.trim();
		if text.is_empty() {
			bail!("empty refspec");
		}
		let (force, body) = match text.strip_prefix('+') {
			Some(rest) => (true, rest),
			None => (false, text),
		};
		if body.contains('*') {
			bail!("wildcard push refspecs are not supported yet: '{text}'");
		}
		let (src, dst) = match body.split_once(':') {
			// `:<dst>` — delete the remote ref.
			Some(("", dst)) if !dst.is_empty() => (None, qualify_dst(dst)),
			Some(("", _)) => bail!("refspec has an empty source and destination: '{text}'"),
			// `<src>:<dst>`. An unqualified destination inherits the source's namespace, so
			// `refs/tags/v1:v2` targets `refs/tags/v2` rather than a `refs/heads/v2` branch.
			Some((src, dst)) if !dst.is_empty() => {
				let src = qualify_src(src);
				(Some(src.clone()), qualify_dst_like(dst, &src))
			}
			// `<src>:` — no destination.
			Some((_, _)) => bail!("refspec has an empty destination: '{text}'"),
			// Bare `HEAD` — git's `push origin HEAD` shorthand: push the current branch to a same-named
			// remote branch. The `HEAD` destination is a sentinel the pusher resolves to the current
			// branch. This shorthand applies *only* to a bare `HEAD`; an explicit `HEAD` destination
			// (`main:HEAD`, `:HEAD`) is the literal `refs/heads/HEAD` via `qualify_dst` above.
			None if body == "HEAD" => (Some("HEAD".to_owned()), "HEAD".to_owned()),
			// `<name>` — push to the same-name remote ref (git DWIM).
			None => (Some(qualify_src(body)), qualify_dst(body)),
		};
		Ok(Self { force, src, dst })
	}
}

/// Qualify a push source: `HEAD` and `refs/*` pass through; a bare `<name>` becomes `refs/heads/<name>`.
fn qualify_src(src: &str) -> String {
	if src == "HEAD" || src.starts_with("refs/") {
		src.to_owned()
	} else {
		format!("refs/heads/{src}")
	}
}

/// Qualify a push destination: `refs/*` passes through; anything else becomes `refs/heads/<name>`. An
/// explicit `HEAD` destination is therefore the literal `refs/heads/HEAD` — only a *bare* `HEAD`
/// refspec (handled in [`PushRefspec::parse`]) is the current-branch shorthand.
fn qualify_dst(dst: &str) -> String {
	if dst.starts_with("refs/") {
		dst.to_owned()
	} else {
		format!("refs/heads/{dst}")
	}
}

/// Qualify a push destination inheriting the (already-qualified) source's namespace: an unqualified
/// destination under a `refs/tags/*` source becomes a `refs/tags/*` ref, otherwise `refs/heads/*`.
/// `refs/*` destinations still pass through unchanged.
fn qualify_dst_like(dst: &str, src: &str) -> String {
	if dst.starts_with("refs/") {
		dst.to_owned()
	} else if src.starts_with("refs/tags/") {
		format!("refs/tags/{dst}")
	} else {
		format!("refs/heads/{dst}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_the_common_forms() {
		assert_eq!(
			PushRefspec::parse("main").unwrap(),
			PushRefspec {
				force: false,
				src: Some("refs/heads/main".to_owned()),
				dst: "refs/heads/main".to_owned(),
			}
		);
		assert_eq!(
			PushRefspec::parse("+HEAD:refs/heads/x").unwrap(),
			PushRefspec {
				force: true,
				src: Some("HEAD".to_owned()),
				dst: "refs/heads/x".to_owned(),
			}
		);
		assert_eq!(
			PushRefspec::parse("dev:release").unwrap(),
			PushRefspec {
				force: false,
				src: Some("refs/heads/dev".to_owned()),
				dst: "refs/heads/release".to_owned(),
			}
		);
		assert_eq!(
			PushRefspec::parse("refs/tags/v1:refs/tags/v1").unwrap().src,
			Some("refs/tags/v1".to_owned())
		);
	}

	#[test]
	fn unqualified_destination_inherits_the_source_namespace() {
		// A `refs/tags/*` source pushes to a tag, not a branch, when the destination is unqualified.
		let spec = PushRefspec::parse("refs/tags/v1:v2").unwrap();
		assert_eq!(spec.src, Some("refs/tags/v1".to_owned()));
		assert_eq!(spec.dst, "refs/tags/v2");
		// A branch source still defaults its unqualified destination to a branch.
		assert_eq!(
			PushRefspec::parse("dev:release").unwrap().dst,
			"refs/heads/release"
		);
	}

	#[test]
	fn bare_head_keeps_head_as_the_destination() {
		// A bare `HEAD` must not become `refs/heads/HEAD`; the destination stays `HEAD` for the pusher
		// to resolve to the current branch (git's `push origin HEAD` shorthand).
		let spec = PushRefspec::parse("HEAD").unwrap();
		assert_eq!(spec.src, Some("HEAD".to_owned()));
		assert_eq!(spec.dst, "HEAD");
	}

	#[test]
	fn explicit_head_destination_is_the_literal_ref() {
		// Only a *bare* `HEAD` is the shorthand — an explicit `HEAD` destination is the literal ref,
		// so `main:HEAD` and `:HEAD` (a deletion) never resolve to the current branch.
		assert_eq!(
			PushRefspec::parse("main:HEAD").unwrap().dst,
			"refs/heads/HEAD"
		);
		assert_eq!(PushRefspec::parse(":HEAD").unwrap().dst, "refs/heads/HEAD");
	}

	#[test]
	fn parses_a_deletion() {
		let spec = PushRefspec::parse(":stale").unwrap();
		assert_eq!(spec.src, None);
		assert_eq!(spec.dst, "refs/heads/stale");
	}

	#[test]
	fn rejects_wildcards_and_empties() {
		assert!(PushRefspec::parse("refs/heads/*:refs/heads/*").is_err());
		assert!(PushRefspec::parse("").is_err());
		assert!(PushRefspec::parse("main:").is_err());
		assert!(PushRefspec::parse(":").is_err());
	}
}
