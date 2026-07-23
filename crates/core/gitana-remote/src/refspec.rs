//! Parsing and matching of Git fetch refspecs (`[+]<src>[:<dst>]`, plus negative `^<pattern>`).
//!
//! A fetch refspec maps a remote (advertised) ref name to a local ref to update. `git` allows at most
//! one `*` wildcard on each side; the text the `*` matches in `<src>` is substituted into `<dst>`. A
//! leading `+` forces the update (a non-fast-forward is allowed). A refspec with no `<dst>` (either no
//! colon, or an empty right-hand side) fetches the ref without updating a local tracking ref. A
//! negative refspec `^<pattern>` excludes any advertised ref its pattern matches.

use anyhow::{Result, bail};

/// Expand an unqualified fetch destination to a local branch, as git does (`foo` → `refs/heads/foo`);
/// a `refs/`-rooted destination is already fully qualified.
fn qualify_destination(dst: &str) -> String {
	if dst.starts_with("refs/") {
		dst.to_owned()
	} else {
		format!("refs/heads/{dst}")
	}
}

/// Reject an exact source that could never match an advertised ref. Git DWIMs a shorthand like `main`
/// to `refs/heads/main`; rather than implement that resolution we require a full ref name (or `HEAD`),
/// so a shorthand is a clear config error instead of a silent no-op. Wildcards match positionally and
/// need no qualification.
fn require_matchable_source(src: &Pattern, text: &str) -> Result<()> {
	if let Pattern::Exact(exact) = src
		&& !exact.starts_with("refs/")
		&& exact != "HEAD"
	{
		bail!("refspec source must be a full ref name or HEAD: '{text}'");
	}
	Ok(())
}

/// One side of a refspec: an exact ref name, or a single-`*` glob split into its fixed prefix/suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
	Exact(String),
	Glob { prefix: String, suffix: String },
}

impl Pattern {
	/// Parse one side of a refspec, rejecting a second `*` (Git allows at most one per side).
	fn parse(text: &str) -> Result<Self> {
		match text.split_once('*') {
			None => Ok(Pattern::Exact(text.to_owned())),
			Some((prefix, rest)) => {
				if rest.contains('*') {
					bail!("refspec pattern has more than one '*': '{text}'");
				}
				Ok(Pattern::Glob {
					prefix: prefix.to_owned(),
					suffix: rest.to_owned(),
				})
			}
		}
	}

	fn is_glob(&self) -> bool {
		matches!(self, Pattern::Glob { .. })
	}

	/// If `name` matches, the text captured by the `*` (empty string for an exact pattern).
	fn match_capture<'a>(&self, name: &'a str) -> Option<&'a str> {
		match self {
			Pattern::Exact(exact) => (exact == name).then_some(""),
			Pattern::Glob { prefix, suffix } => (name.len() >= prefix.len() + suffix.len()
				&& name.starts_with(prefix.as_str())
				&& name.ends_with(suffix.as_str()))
			.then(|| &name[prefix.len()..name.len() - suffix.len()]),
		}
	}

	/// Substitute a captured `*` back into this pattern to form a concrete ref name.
	fn substitute(&self, capture: &str) -> String {
		match self {
			Pattern::Exact(exact) => exact.clone(),
			Pattern::Glob { prefix, suffix } => format!("{prefix}{capture}{suffix}"),
		}
	}
}

/// A parsed fetch refspec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refspec {
	/// A `+`-prefixed refspec forces the update (allows a non-fast-forward). Always `false` for a
	/// negative refspec.
	pub force: bool,
	/// A `^<pattern>` refspec: it names refs to *exclude*, and never maps to a destination.
	pub negative: bool,
	/// The remote (source) side pattern.
	src: Pattern,
	/// The local (destination) side pattern; `None` when the refspec has no tracking destination
	/// (an empty right-hand side, no colon at all, or a negative refspec).
	dst: Option<Pattern>,
}

impl Refspec {
	/// Parse a single refspec string as configured in `remote.<name>.fetch`.
	pub fn parse(text: &str) -> Result<Self> {
		let text = text.trim();
		if text.is_empty() {
			bail!("empty refspec");
		}
		// Negative refspec: `^<pattern>`, excluding matching refs. No force, no destination.
		if let Some(pattern) = text.strip_prefix('^') {
			if pattern.contains(':') {
				bail!("a negative refspec takes no destination: '{text}'");
			}
			if pattern.is_empty() {
				bail!("empty negative refspec");
			}
			let src = Pattern::parse(pattern)?;
			require_matchable_source(&src, text)?;
			return Ok(Refspec {
				force: false,
				negative: true,
				src,
				dst: None,
			});
		}

		let (force, body) = match text.strip_prefix('+') {
			Some(rest) => (true, rest),
			None => (false, text),
		};
		// `<src>:<dst>`; with no colon the whole thing is the source and there is no destination.
		let (src_text, dst_text) = match body.split_once(':') {
			Some((src, dst)) => (src, Some(dst)),
			None => (body, None),
		};
		if src_text.is_empty() {
			bail!("refspec has an empty source: '{text}'");
		}
		let src = Pattern::parse(src_text)?;
		require_matchable_source(&src, text)?;
		// An empty destination (`<src>:`) means "no tracking ref", like having no colon at all. git
		// DWIMs only an *exact* unqualified destination to a local branch (`foo` → `refs/heads/foo`); a
		// wildcard destination must be fully qualified, else git ignores the produced "funny" refs
		// (which `destination` drops).
		let dst = match dst_text {
			Some(dst) if !dst.is_empty() => {
				let qualified = if dst.contains('*') {
					dst.to_owned()
				} else {
					qualify_destination(dst)
				};
				Some(Pattern::parse(&qualified)?)
			}
			_ => None,
		};
		// Git requires the `*` to appear on both sides or neither — and a wildcard source needs a
		// wildcard destination, so `refs/heads/*` (or `refs/heads/*:`) with no destination is invalid.
		match &dst {
			Some(dst) if src.is_glob() != dst.is_glob() => {
				bail!("refspec must have '*' on both sides or neither: '{text}'");
			}
			None if src.is_glob() => {
				bail!("a wildcard refspec needs a wildcard destination: '{text}'");
			}
			_ => {}
		}
		Ok(Refspec {
			force,
			negative: false,
			src,
			dst,
		})
	}

	/// The local ref that advertised source ref `name` maps to under this refspec, or `None` when the
	/// refspec does not match `name` or has no tracking destination (empty dst, or a negative refspec).
	pub fn destination(&self, name: &str) -> Option<String> {
		if self.negative {
			return None;
		}
		let capture = self.src.match_capture(name)?;
		let dst = self.dst.as_ref()?.substitute(capture);
		// git ignores a produced destination outside `refs/` (a "funny ref", e.g. from a wildcard
		// destination that was never qualified); drop it rather than write an invalid local ref.
		dst.starts_with("refs/").then_some(dst)
	}

	/// Whether this is a negative refspec whose pattern matches `name` (so `name` is excluded).
	pub fn excludes(&self, name: &str) -> bool {
		self.negative && self.src.match_capture(name).is_some()
	}

	/// Whether this positive refspec's *source* pattern selects advertised ref `name` — i.e. the refspec
	/// fetches it, even when it has no tracking destination (a source-only `refs/heads/main` fetch, which
	/// [`destination`](Self::destination) reports as `None`). Always `false` for a negative refspec.
	pub fn matches_source(&self, name: &str) -> bool {
		!self.negative && self.src.match_capture(name).is_some()
	}

	/// The exact source ref this refspec names, if its source is not a wildcard. git errors when such a
	/// ref is not advertised (`couldn't find remote ref …`); a wildcard matching nothing is not an error.
	pub fn exact_source(&self) -> Option<&str> {
		match &self.src {
			Pattern::Exact(name) => Some(name),
			Pattern::Glob { .. } => None,
		}
	}

	/// The fixed prefix of this positive refspec's *destination* glob, for enumerating the local tracking
	/// refs that fall under it (the candidates a `--prune` fetch considers). `None` when the destination
	/// is absent or exact — only a wildcard destination owns a namespace to prune. For
	/// `+refs/heads/*:refs/remotes/origin/*` this is `Some("refs/remotes/origin/")`.
	pub fn destination_glob_prefix(&self) -> Option<&str> {
		if self.negative {
			return None;
		}
		match self.dst.as_ref()? {
			Pattern::Glob { prefix, .. } => Some(prefix),
			Pattern::Exact(_) => None,
		}
	}

	/// Whether local tracking ref `tracking` is covered by this positive refspec's destination glob — i.e.
	/// this refspec is the one that would produce `tracking` from some advertised ref. A `tracking` that
	/// is covered but that no advertised ref maps to is a prune candidate under this refspec.
	pub fn covers_destination(&self, tracking: &str) -> bool {
		if self.negative {
			return false;
		}
		matches!(self.dst.as_ref(), Some(dst @ Pattern::Glob { .. }) if dst.match_capture(tracking).is_some())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_the_default_fetch_refspec() {
		let spec = Refspec::parse("+refs/heads/*:refs/remotes/origin/*").unwrap();
		assert!(spec.force);
		assert!(!spec.negative);
		assert_eq!(
			spec.destination("refs/heads/main").as_deref(),
			Some("refs/remotes/origin/main")
		);
		// A nested branch name: the whole `*` capture (including `/`) is substituted.
		assert_eq!(
			spec.destination("refs/heads/feature/x").as_deref(),
			Some("refs/remotes/origin/feature/x")
		);
		// A non-branch ref does not match.
		assert_eq!(spec.destination("refs/tags/v1"), None);
	}

	#[test]
	fn destination_glob_prefix_and_coverage_drive_prune() {
		// The default fetch refspec owns the `refs/remotes/origin/` namespace to prune.
		let wild = Refspec::parse("+refs/heads/*:refs/remotes/origin/*").unwrap();
		assert_eq!(wild.destination_glob_prefix(), Some("refs/remotes/origin/"));
		assert!(wild.covers_destination("refs/remotes/origin/feature"));
		assert!(wild.covers_destination("refs/remotes/origin/feature/x"));
		// A ref outside the destination namespace is not covered.
		assert!(!wild.covers_destination("refs/heads/main"));
		assert!(!wild.covers_destination("refs/remotes/upstream/main"));

		// An exact destination owns no namespace: it maps a single ref, so a prune walks nothing.
		let exact = Refspec::parse("refs/heads/main:refs/remotes/origin/main").unwrap();
		assert_eq!(exact.destination_glob_prefix(), None);
		assert!(!exact.covers_destination("refs/remotes/origin/main"));

		// A source-only refspec (no destination) and a negative refspec prune nothing.
		let source_only = Refspec::parse("refs/heads/main").unwrap();
		assert_eq!(source_only.destination_glob_prefix(), None);
		assert!(!source_only.covers_destination("refs/remotes/origin/main"));
		let negative = Refspec::parse("^refs/heads/wip/*").unwrap();
		assert_eq!(negative.destination_glob_prefix(), None);
		assert!(!negative.covers_destination("refs/remotes/origin/wip"));
	}

	#[test]
	fn matches_source_selects_even_without_a_destination() {
		// A source-only refspec (no tracking destination) still *selects* its source — used to pick a
		// shallow fetch's deepen roots, where `destination` would wrongly drop it.
		let source_only = Refspec::parse("refs/heads/main").unwrap();
		assert_eq!(source_only.destination("refs/heads/main"), None);
		assert!(source_only.matches_source("refs/heads/main"));
		assert!(!source_only.matches_source("refs/heads/dev"));
		// A wildcard positive refspec selects every matching source.
		let wild = Refspec::parse("+refs/heads/*:refs/remotes/origin/*").unwrap();
		assert!(wild.matches_source("refs/heads/feature/x"));
		assert!(!wild.matches_source("refs/tags/v1"));
		// A negative refspec never selects a source (it only excludes).
		assert!(
			!Refspec::parse("^refs/heads/large")
				.unwrap()
				.matches_source("refs/heads/large")
		);
	}

	#[test]
	fn non_forced_refspec_has_no_plus() {
		let spec = Refspec::parse("refs/heads/*:refs/remotes/origin/*").unwrap();
		assert!(!spec.force);
		assert_eq!(
			spec.destination("refs/heads/main").as_deref(),
			Some("refs/remotes/origin/main")
		);
	}

	#[test]
	fn exact_source_is_reported_only_for_non_wildcard_sources() {
		assert_eq!(
			Refspec::parse("refs/heads/main:refs/remotes/origin/main")
				.unwrap()
				.exact_source(),
			Some("refs/heads/main")
		);
		assert_eq!(
			Refspec::parse("+refs/heads/*:refs/remotes/origin/*")
				.unwrap()
				.exact_source(),
			None
		);
	}

	#[test]
	fn exact_refspec_maps_one_ref() {
		let spec = Refspec::parse("refs/heads/main:refs/remotes/origin/trunk").unwrap();
		assert_eq!(
			spec.destination("refs/heads/main").as_deref(),
			Some("refs/remotes/origin/trunk")
		);
		assert_eq!(spec.destination("refs/heads/other"), None);
	}

	#[test]
	fn empty_and_missing_destination_map_nowhere() {
		for text in ["refs/heads/main:", "refs/heads/main"] {
			let spec = Refspec::parse(text).unwrap();
			assert_eq!(spec.destination("refs/heads/main"), None, "for '{text}'");
		}
	}

	#[test]
	fn negative_refspec_excludes_matching_refs() {
		let spec = Refspec::parse("^refs/heads/wip/*").unwrap();
		assert!(spec.negative);
		assert!(spec.excludes("refs/heads/wip/experiment"));
		assert!(!spec.excludes("refs/heads/main"));
		// A negative refspec never produces a destination.
		assert_eq!(spec.destination("refs/heads/wip/experiment"), None);
	}

	#[test]
	fn exact_unqualified_destination_is_expanded_to_a_local_branch() {
		// git shorthand: an *exact* destination not rooted at `refs/` names a local branch.
		let exact = Refspec::parse("refs/heads/main:foo").unwrap();
		assert_eq!(
			exact.destination("refs/heads/main").as_deref(),
			Some("refs/heads/foo")
		);
	}

	#[test]
	fn wildcard_destination_outside_refs_is_ignored() {
		// git does not DWIM a wildcard destination; `mirror/main` is a "funny ref" it ignores.
		let wildcard = Refspec::parse("refs/heads/*:mirror/*").unwrap();
		assert_eq!(wildcard.destination("refs/heads/main"), None);
	}

	#[test]
	fn unqualified_source_is_rejected() {
		// git DWIMs `main` → `refs/heads/main`; we require a full ref name rather than silently
		// matching nothing.
		assert!(Refspec::parse("main:refs/remotes/origin/main").is_err());
		assert!(Refspec::parse("^wip").is_err());
		// `HEAD` and fully-qualified sources are fine.
		assert!(Refspec::parse("HEAD:refs/remotes/origin/head").is_ok());
		assert!(Refspec::parse("refs/heads/main:refs/remotes/origin/main").is_ok());
	}

	#[test]
	fn mid_pattern_wildcard_is_supported() {
		let spec = Refspec::parse("refs/heads/release-*:refs/remotes/origin/rel-*").unwrap();
		assert_eq!(
			spec.destination("refs/heads/release-1.0").as_deref(),
			Some("refs/remotes/origin/rel-1.0")
		);
		// The fixed prefix must match.
		assert_eq!(spec.destination("refs/heads/main"), None);
	}

	#[test]
	fn rejects_malformed_refspecs() {
		// Two wildcards on one side.
		assert!(Refspec::parse("refs/heads/*/*:refs/remotes/origin/*").is_err());
		// Wildcard on only one side.
		assert!(Refspec::parse("refs/heads/*:refs/remotes/origin/main").is_err());
		assert!(Refspec::parse("refs/heads/main:refs/remotes/origin/*").is_err());
		// Wildcard source with no destination at all (git: "invalid refspec").
		assert!(Refspec::parse("refs/heads/*").is_err());
		assert!(Refspec::parse("refs/heads/*:").is_err());
		// Empty source.
		assert!(Refspec::parse(":refs/remotes/origin/main").is_err());
		// Empty / destination-bearing negative.
		assert!(Refspec::parse("^").is_err());
		assert!(Refspec::parse("^refs/heads/*:refs/x").is_err());
		// Empty overall.
		assert!(Refspec::parse("   ").is_err());
	}
}
