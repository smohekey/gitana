//! Pathspec normalisation shared by `add` and `restore`.
//!
//! A pathspec from the command line is interpreted relative to the directory the user invoked
//! the command from. `normalize` turns it into a canonical work-tree-relative path: it combines
//! the caller's `prefix` (a `/`-joined work-tree-relative subdirectory, empty at the root) with
//! the spec, then resolves `.` and `..` components against it. The current-dir forms `.` and
//! `./` collapse to the prefix itself (everything under the caller's directory). An empty
//! pathspec (`""`), and a `..` that climbs above the work-tree root, are rejected the way stock
//! git rejects them. A leading `/` (an absolute path) is also rejected, but here we differ from
//! git: git accepts an absolute path that points inside the work tree (relativising it), whereas
//! we only support worktree-relative pathspecs for now. Silently stripping the leading `/` and
//! treating it as relative would act on the wrong file, so rejecting is the safe choice.
//!
//! A trailing slash (`sub/`) or a final `.` component (`sub/.`) is reported via `dir_only`:
//! such a spec must resolve to a directory, the way `git checkout -- a.txt/` and
//! `git checkout -- a.txt/.` are rejected for a file. A final `..` is not directory-only — it
//! resolves to a parent the way git accepts `a.txt/..` (the directory above the file).

use crate::WorktreeError;

/// A parsed pathspec: its canonical worktree-relative form (the caller's `prefix` applied, `.`/`..`
/// resolved) plus how it matches. A pathspec with a glob metacharacter (`*`/`?`/`[`) matches by
/// wildmatch (git's default pathspec globbing — the wildcards **cross `/`**, and `**` is not special);
/// a literal one matches a path exactly or as a leading directory (its contents). A leading magic
/// prefix (`:(...)` long form, or the short `:/` / `:!` / `:^`) overrides this: `top` resolves from the
/// repo root (ignoring the prefix), `literal` disables globbing, `glob` forces FNM_PATHNAME globbing
/// (`*` stops at `/`, `**` spans), `icase` folds ASCII case, and `exclude` makes it a *negative*
/// pathspec (see [`PathspecSet`]). Matching a single spec against a path is [`matches`](Self::matches).
pub(crate) struct Pathspec {
	/// The canonical worktree-relative pattern (empty means the work-tree root).
	normalized: String,
	/// When `normalized` is empty, whether it means "the whole tree" (`.`, a bare `:` / `:/` / `:(top)`)
	/// rather than "matches nothing". A magic prefix with a *non-empty* path that resolves to the root —
	/// `:/.` or `:(top).` — is the latter: git reports it unmatched for `rm`/`restore` and a no-op for
	/// `add`, NOT a whole-tree match (probed vs git 2.50.1). Only meaningful when `normalized` is empty.
	matches_root: bool,
	/// The spec ended in a slash or a final `.` component, so it may only match a directory.
	dir_only: bool,
	/// The pattern matches by glob (a metacharacter, or `:(glob)`) rather than the literal rule.
	wildcard: bool,
	/// Glob in FNM_PATHNAME mode (`:(glob)`): `*`/`?`/`[]` do not cross `/`, and `**` spans directories.
	/// The default (`false`) is git's plain pathspec glob where the wildcards cross `/`.
	pathname: bool,
	/// Fold ASCII case (`:(icase)`).
	icase: bool,
	/// A negative (`:(exclude)` / `:!` / `:^`) pathspec — it *subtracts* from the positive set.
	exclude: bool,
}

/// The keywords of a pathspec magic prefix.
#[derive(Default)]
struct Magic {
	top: bool,
	literal: bool,
	glob: bool,
	icase: bool,
	exclude: bool,
}

/// Split a leading pathspec magic prefix from `spec`, returning the keywords and the remaining path
/// portion. Recognises the long form `:(top,literal,glob,icase,exclude)…` (an unknown keyword — e.g.
/// `attr:`, which needs `.gitattributes` — is rejected) and the short forms `:/` (top) and `:!` / `:^`
/// (exclude). A `:` not followed by recognised magic is treated as a literal path (git accepts a bare
/// `:` as the repo top; `:x` matches nothing there, and a literal `:x` likewise matches no such file).
fn strip_magic(spec: &str) -> Result<(Magic, &str), WorktreeError> {
	let mut magic = Magic::default();
	if let Some(rest) = spec.strip_prefix(":(") {
		let close = rest
			.find(')')
			.ok_or_else(|| WorktreeError::InvalidPathspecMagic(spec.to_owned()))?;
		for word in rest[..close].split(',') {
			match word {
				"" => {}
				"top" => magic.top = true,
				"literal" => magic.literal = true,
				"glob" => magic.glob = true,
				"icase" => magic.icase = true,
				"exclude" => magic.exclude = true,
				_ => return Err(WorktreeError::InvalidPathspecMagic(spec.to_owned())),
			}
		}
		Ok((magic, &rest[close + 1..]))
	} else if let Some(rest) = spec.strip_prefix(':') {
		// Short form: the leading `:` is magic. Consume the signature bytes `/` (top) and `!`/`^` (exclude);
		// a `:` terminates the magic (`::x`). git reserves other punctuation as short-magic *signatures* and
		// aborts on an unrecognised one (`:@x` → "unimplemented magic"), rather than taking it as a literal
		// path — so we reject those. A non-signature byte (alnum, `.`, `$`, a glob metacharacter, …) ends
		// the magic and begins the path: `:x` is path `x` with empty magic, `:/x` a top-relative `x`
		// (probed vs git 2.50.1).
		let bytes = rest.as_bytes();
		let mut i = 0;
		while i < bytes.len() {
			match bytes[i] {
				b'/' => magic.top = true,
				b'!' | b'^' => magic.exclude = true,
				b':' => break, // an explicit terminator (`::x`)
				b if is_reserved_magic_signature(b) => {
					return Err(WorktreeError::InvalidPathspecMagic(spec.to_owned()));
				}
				_ => break, // a non-signature byte — the path starts here
			}
			i += 1;
		}
		let after = &rest[i..];
		Ok((magic, after.strip_prefix(':').unwrap_or(after)))
	} else {
		Ok((magic, spec))
	}
}

/// Whether byte `c`, following a leading `:`, is one of git's *reserved but unimplemented* short-magic
/// signatures — punctuation git treats as a magic mnemonic and aborts on (`:@x`, `:#x`, `:-x`, …). The
/// implemented signatures `/`, `!`, `^` and the terminator `:` are handled by the caller; everything
/// listed here has no meaning yet and so is rejected rather than taken as a literal path. Any other byte
/// (alphanumerics, `.`, `$`, `+`, glob metacharacters `*?[\`, `|{}()`, …) ends the magic and begins the
/// path. The exact set was probed byte-by-byte against git 2.50.1.
fn is_reserved_magic_signature(c: u8) -> bool {
	matches!(
		c,
		b'"'
			| b'#'
			| b'%'
			| b'&'
			| b'\''
			| b','
			| b'-'
			| b';'
			| b'<'
			| b'='
			| b'>'
			| b'@'
			| b'_'
			| b'`'
			| b'~'
	)
}

/// Byte-slice equality, folding ASCII case when `icase`.
fn bytes_eq(a: &[u8], b: &[u8], icase: bool) -> bool {
	if icase {
		a.eq_ignore_ascii_case(b)
	} else {
		a == b
	}
}

/// Whether `pat` needs the wildmatch engine, matching git: a `*` or `?` always, a `\` (which escapes the
/// next byte — `foo\[` selects the file `foo[`), or a `[` that a `]` closes. An unterminated `[` (e.g.
/// `foo[`) is an ordinary literal character, not a class.
fn has_wildcard(pat: &str) -> bool {
	// Single linear pass (not a rescan of the tail at every `[`, which is quadratic on `[[[[…`): a `[`
	// is a wildcard only once a `]` closes it somewhere later.
	let mut open_bracket = false;
	for &b in pat.as_bytes() {
		match b {
			b'*' | b'?' | b'\\' => return true,
			b'[' => open_bracket = true,
			b']' if open_bracket => return true,
			_ => {}
		}
	}
	false
}

impl Pathspec {
	/// Parse `spec` (from the command line, relative to `prefix`) into a matcher. Rejects the same
	/// forms [`normalize`] does (empty, absolute, an escape above the root), plus an unknown or
	/// incompatible magic.
	pub(crate) fn parse(spec: &str, prefix: &str) -> Result<Self, WorktreeError> {
		let (magic, rest) = strip_magic(spec)?;
		// `literal` and `glob` are mutually exclusive — git rejects the combination.
		if magic.literal && magic.glob {
			return Err(WorktreeError::InvalidPathspecMagic(spec.to_owned()));
		}
		// `:(top)` / `:/` resolve from the repository root, ignoring the invocation prefix.
		let effective_prefix = if magic.top { "" } else { prefix };
		// A magic prefix with an empty path (`:`, `:/`, `:(top)`, `:(icase)`) matches the caller's directory
		// — the *effective prefix* — not an empty-pathspec error. That prefix is the repo root only for
		// `:(top)`/`:/`; from a subdirectory, `:` / `:(icase)` / `:(glob)` stay scoped to it (probed vs git
		// 2.50.1: `git -C sub rm -r :` removes `sub/*`, `:/` the whole tree). A bare empty spec (no magic)
		// still errors via `normalize`.
		let had_magic = rest.len() < spec.len();
		let (normalized, dir_only) = if rest.is_empty() && had_magic {
			(effective_prefix.to_owned(), false)
		} else {
			normalize(rest, effective_prefix)?
		};
		// An empty `normalized` means "the whole tree" ONLY when the spec was a bare magic (empty path) or a
		// non-magic root form (`.`); a magic prefix carrying a non-empty path that merely *resolved* to the
		// root (`:/.`, `:(top).`) matches nothing instead — git reports it unmatched, never a whole-tree hit.
		let matches_root = normalized.is_empty() && (rest.is_empty() || !magic.top);
		// `:(glob)` selects FNM_PATHNAME globbing; the default lets wildcards cross `/`. Whether the spec
		// is *treated* as a glob depends on real metacharacters, so a plain path (or `:(glob)src`) keeps
		// the literal leading-directory rule, while `:(literal)` never globs.
		let wildcard = !magic.literal && has_wildcard(&normalized);
		Ok(Self {
			normalized,
			matches_root,
			dir_only,
			wildcard,
			pathname: magic.glob,
			icase: magic.icase,
			exclude: magic.exclude,
		})
	}

	/// Whether `path` matched this spec as a *leading directory* expansion — `path` lies under the spec's
	/// literal spelling treated as a directory (`p == "<spec>/…"`), rather than an exact or glob file
	/// match. `rm` requires `-r` for such an expansion, for a literal *and* a wildcard spec (probed vs git
	/// 2.50.1: `rm 'a?'` on the literally-named directory `a?/` needs `-r`, while a glob file match like
	/// `rm 'a?/f'` selecting `ax/f` does not). Folds ASCII case under `:(icase)`.
	pub(crate) fn expands_directory(&self, path: &str) -> bool {
		// The root pathspec (`.`, `./`, `:`, `:/`, a magic-only form — `normalized` is empty) matches every
		// path as the whole-tree expansion, so `rm .` is recursive and git rejects it without `-r`.
		if self.normalized.is_empty() {
			return true;
		}
		let (n, p) = (self.normalized.as_bytes(), path.as_bytes());
		p.len() > n.len() && p[n.len()] == b'/' && bytes_eq(&p[..n.len()], n, self.icase)
	}

	/// Whether this is a negative (`:(exclude)` / `:!` / `:^`) pathspec.
	pub(crate) fn is_exclude(&self) -> bool {
		self.exclude
	}

	/// Whether this positive pathspec can never match a path — a magic prefix whose non-empty path merely
	/// resolved to the root (`:/.`, `:(top).`). It selects nothing: `rm`/`restore` report it unmatched (via
	/// [`matches`](Self::matches) returning false), and `add` treats it as a no-op.
	pub(crate) fn is_never_matching(&self) -> bool {
		self.normalized.is_empty() && !self.matches_root
	}

	/// Whether this pathspec folds ASCII case (`:(icase)`). `add` routes such a spec through the
	/// walk-and-filter path so a match resolves to the actual worktree path, not the spec's spelling.
	pub(crate) fn is_icase(&self) -> bool {
		self.icase
	}

	/// Whether this pathspec carries `:(glob)` magic (FNM_PATHNAME wildmatch). git treats a `:(glob)`
	/// pathspec as a glob even without metacharacters, so `:(glob)ign/new` naming an untracked ignored
	/// file is git's "did not match", NOT the literal path it would otherwise resolve to (probed vs git
	/// 2.50.1). `add` uses this to distinguish it from a plain literal.
	pub(crate) fn is_glob(&self) -> bool {
		self.pathname
	}

	/// The canonical worktree-relative pattern (empty = the root).
	pub(crate) fn as_str(&self) -> &str {
		&self.normalized
	}

	/// Whether this is a literal (non-glob) pathspec. Only a literal pathspec expands a leading
	/// directory to its contents, and only such an expansion requires `rm -r`; a glob matches individual
	/// files (probed vs git 2.50.1 — `rm '*.rs'` needs no `-r`).
	pub(crate) fn is_literal(&self) -> bool {
		!self.wildcard
	}

	/// Whether the spec required a directory (a trailing slash or a final `.` component).
	pub(crate) fn dir_only(&self) -> bool {
		self.dir_only
	}

	/// The literal directory prefix a glob is rooted at — everything before the last `/` that precedes
	/// the first wildcard (empty = the work-tree root). `add` walks only this subtree for a glob rather
	/// than the whole tree (`src/*.rs` walks `src`, `*.rs` walks the root). A `\` (which escapes the next
	/// byte, e.g. an escaped separator `dir\/foo`) also ends the literal prefix, so the derivation never
	/// treats the backslash as part of a real directory name and skips the walk — it falls back to the
	/// root, and `matches` filters (probed vs git 2.50.1: `add 'dir\/foo'` stages `dir/foo`).
	pub(crate) fn base_dir(&self) -> &str {
		// Under `:(icase)` the directory casing is unknown, so walk from the root and let `matches` fold.
		if self.icase {
			return "";
		}
		let first_wild = self
			.normalized
			.bytes()
			.position(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
			.unwrap_or(self.normalized.len());
		match self.normalized[..first_wild].rfind('/') {
			Some(i) => &self.normalized[..i],
			None => "",
		}
	}

	/// The fixed leading path of the spec — everything before its first wildcard — INCLUDING any final
	/// component (unlike [`base_dir`], which drops it) and regardless of `:(icase)` (the path structure is
	/// still known even when the casing is not). `sub/*` → `sub/`, `:(icase)sub/new` → `sub/new`, `*` → ``.
	/// Used to detect a spec rooted inside a tracked submodule (`add sub/*` is git's "is in submodule").
	pub(crate) fn rooted_prefix(&self) -> &str {
		let first_wild = self
			.normalized
			.bytes()
			.position(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
			.unwrap_or(self.normalized.len());
		&self.normalized[..first_wild]
	}

	/// Whether this pathspec matches the worktree-relative `path`. A glob uses git's default pathspec
	/// wildmatch (`*`/`?`/`[]` cross `/`, `**` not special — probed against git 2.50.1); a literal spec
	/// matches `path` exactly (unless it required a directory) or as a leading directory of `path`. A
	/// glob that merely matches a directory does **not** pull in its contents (git only expands a literal
	/// leading directory), so no directory-prefix rule is applied to a glob.
	pub(crate) fn matches(&self, path: &str) -> bool {
		if self.normalized.is_empty() {
			// The whole tree (`.`, bare `:` / `:/`) — or, for a magic path that only *resolved* to the root
			// (`:/.`), nothing at all.
			return self.matches_root;
		}
		let (n, p) = (self.normalized.as_bytes(), path.as_bytes());
		// Exact (unless the spec required a directory), or a leading directory of `path` — folding ASCII
		// case under `:(icase)`. The `/` boundary check keeps the prefix slice byte-aligned. git applies
		// this literal pass to the pattern's raw spelling for *every* spec, so a wildcard spec also selects
		// the literally-named directory it spells: `a?` matches the real directory `a?/`'s contents even
		// though the 2-char glob cannot full-match a longer path, and only its literal pass fails on a
		// dangling backslash (`?\` selects just `?\`) — both probed vs git 2.50.1.
		let exact = !self.dir_only && bytes_eq(p, n, self.icase);
		let under = p.len() > n.len() && p[n.len()] == b'/' && bytes_eq(&p[..n.len()], n, self.icase);
		if self.wildcard {
			// A directory-only glob (`src*/`) matches no file entry — git requires the glob to name a
			// directory, and index/worktree paths are files (probed vs git 2.50.1: `src*/` matches nothing).
			// The exception is a trailing pathname globstar (`:(glob)a**/`): `**` can consume zero directories,
			// so the pattern matches the FILE `a` — but ONLY that. Run the wildmatch with the trailing `/`
			// restored (it went into `dir_only`), NOT the stripped `a**` (which, as a trailing globstar, would
			// match every `a`-prefixed path); `a**/` matches `a` alone. Probed vs git 2.50.1.
			let glob = if self.dir_only {
				self.pathname
					&& self.normalized.ends_with("**")
					&& crate::ignore::glob_match(
						format!("{}/", self.normalized).as_bytes(),
						p,
						self.icase,
						self.pathname,
					)
			} else {
				crate::ignore::glob_match(n, p, self.icase, self.pathname)
			};
			glob || exact || under
		} else {
			exact || under
		}
	}
}

/// A parsed set of pathspecs, split into positives and negatives. A path is *selected* when it matches
/// at least one positive (or there are no positives) **and** matches no negative (`:(exclude)` / `:!` /
/// `:^`) — git's rule, so `. :!vendor` stages everything except `vendor`. Each positive remembers
/// whether it matched anything, for git's "did not match any files" error.
pub(crate) struct PathspecSet {
	// `AtomicBool` (not `Cell`) so `PathspecSet` stays `Sync`: `WorkTree::add` holds a `&PathspecSet`
	// across awaits, and callers `tokio::spawn` that future, so it must remain `Send`.
	positive: Vec<(String, Pathspec, std::sync::atomic::AtomicBool)>,
	negative: Vec<Pathspec>,
}

// Guard the `Send`-ness of `WorkTree::add` at compile time: it borrows a `PathspecSet` across awaits, so
// the set must be `Sync`. A regression to `Cell` would fail this assertion (and break `tokio::spawn`).
const _: fn() = || {
	fn assert_sync<T: Sync>() {}
	assert_sync::<PathspecSet>();
};

impl PathspecSet {
	/// Parse each raw spec (relative to `prefix`) into the set, routing `:(exclude)` ones to the
	/// negatives. Rejects the same forms [`Pathspec::parse`] does (empty/absolute/escape/unknown magic).
	pub(crate) fn parse(specs: &[&str], prefix: &str) -> Result<Self, WorktreeError> {
		let mut positive = Vec::new();
		let mut negative = Vec::new();
		for &spec in specs {
			let parsed = Pathspec::parse(spec, prefix)?;
			if parsed.is_exclude() {
				negative.push(parsed);
			} else {
				positive.push((
					spec.to_owned(),
					parsed,
					std::sync::atomic::AtomicBool::new(false),
				));
			}
		}
		Ok(Self { positive, negative })
	}

	/// Whether `path` is excluded by a negative pathspec.
	pub(crate) fn is_excluded(&self, path: &str) -> bool {
		self.negative.iter().any(|negative| negative.matches(path))
	}

	/// Whether `path` is selected by the set (matches a positive — or there are none — and no negative).
	/// Records the positives that matched, for [`unmatched`](Self::unmatched).
	pub(crate) fn matches(&self, path: &str) -> bool {
		let mut positive_hit = self.positive.is_empty();
		for (_, pathspec, matched) in &self.positive {
			if pathspec.matches(path) {
				matched.store(true, std::sync::atomic::Ordering::Relaxed);
				positive_hit = true;
			}
		}
		positive_hit && !self.is_excluded(path)
	}

	/// The original text of the first positive pathspec that matched nothing (git's "did not match any
	/// files"), or `None` if every positive matched. Call after iterating all candidate paths through
	/// [`matches`](Self::matches).
	pub(crate) fn unmatched(&self) -> Option<&str> {
		self
			.positive
			.iter()
			.find(|(_, _, matched)| !matched.load(std::sync::atomic::Ordering::Relaxed))
			.map(|(spec, _, _)| spec.as_str())
	}

	/// Every pathspec in the set — positives then negatives — for consumers that inspect each element
	/// regardless of polarity. git's ignored-path advisory fires for a *negative* pathspec too (`add
	/// :!ign/x` advises `ign`), so the advisory pass walks all elements, not just the positives.
	pub(crate) fn all(&self) -> impl Iterator<Item = &Pathspec> {
		self
			.positive
			.iter()
			.map(|(_, pathspec, _)| pathspec)
			.chain(self.negative.iter())
	}

	/// The positive pathspecs paired with their original spec text — for consumers (like `rm`) that need
	/// per-spec handling beyond plain selection.
	pub(crate) fn positives(&self) -> impl Iterator<Item = (&str, &Pathspec)> {
		self
			.positive
			.iter()
			.map(|(spec, pathspec, _)| (spec.as_str(), pathspec))
	}

	/// Whether the set has no positive pathspecs (only negatives, or empty) — then every non-excluded
	/// candidate is selected.
	pub(crate) fn is_positive_empty(&self) -> bool {
		self.positive.is_empty()
	}
}

/// Returns the canonical worktree-relative path together with `dir_only` (the spec ended in a
/// slash or a `.` component and so may only match a directory).
pub(crate) fn normalize(spec: &str, prefix: &str) -> Result<(String, bool), WorktreeError> {
	if spec.starts_with('/') {
		return Err(WorktreeError::AbsolutePathspec(spec.to_owned()));
	}

	// Resolve the spec against the (already-canonical) prefix, applying `.`/`..` as we go.
	let mut stack: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
	let mut named_a_path = false;
	let mut had_dot = false;
	for part in spec.split('/') {
		match part {
			"" => {}
			"." => had_dot = true,
			".." => {
				if stack.pop().is_none() {
					// Climbs above the work-tree root (e.g. `../x` at the root): outside the repo.
					return Err(WorktreeError::UnsafePath(spec.to_owned()));
				}
				named_a_path = true;
			}
			other => {
				stack.push(other);
				named_a_path = true;
			}
		}
	}

	// A spec of `.` / `./` means "everything under here"; `""` / `/` name nothing at all.
	if !named_a_path && !had_dot {
		return Err(WorktreeError::EmptyPathspec);
	}
	// A trailing slash, or a final `.` component (e.g. `a.txt/.`), means a directory is required.
	let last_named = spec.rsplit('/').find(|part| !part.is_empty());
	let dir_only = spec.ends_with('/') || last_named == Some(".");
	Ok((stack.join("/"), dir_only))
}

#[cfg(test)]
mod tests {
	use super::{Pathspec, PathspecSet};
	use crate::WorktreeError;

	fn ps(spec: &str) -> Pathspec {
		Pathspec::parse(spec, "").unwrap()
	}

	#[test]
	fn only_top_magic_dot_matches_nothing_others_match_all() {
		// ONLY a TOP-magic path resolving to the root (`:/.`, `:(top).`) matches nothing — git reports it
		// unmatched. Every other root form matches the whole tree: `.`, a bare `:` / `:/` / `:(top)`, and a
		// NON-top magic dot (`:.`, `:(icase).`, `:(exclude).` — the last as a negative that excludes
		// everything). Probed vs git 2.50.1.
		for spec in [":/.", ":(top).", ":/./"] {
			let p = Pathspec::parse(spec, "").unwrap();
			assert!(p.is_never_matching(), "{spec} should match nothing");
			assert!(!p.matches("a"), "{spec} must not match `a`");
			assert!(!p.matches("sub/b"));
		}
		for spec in [
			":/",
			":(top)",
			".",
			":.",
			":(icase).",
			":(exclude).",
			":(glob).",
			":(literal).",
		] {
			let p = Pathspec::parse(spec, "").unwrap();
			assert!(!p.is_never_matching(), "{spec} matches the whole tree");
			assert!(p.matches("a"), "{spec} should match `a`");
			assert!(p.matches("sub/b"));
		}
		// `:(exclude).` matching everything means it excludes everything: the set selects nothing.
		let set = PathspecSet::parse(&[":(exclude)."], "").unwrap();
		assert!(!set.matches("a"));
		assert!(!set.matches("sub/b"));
	}

	#[test]
	fn glob_trailing_globstar_slash_matches_only_the_bare_file() {
		// A trailing pathname globstar `:(glob)a**/` matches the FILE `a` — `**` consumes zero directories —
		// but ONLY `a`, not every `a`-prefixed path: `aa` and `a/file` are NOT matched (probed vs git 2.50.1).
		// Contrast `a**` (no slash), which as a trailing globstar matches all of them.
		let dir = ps(":(glob)a**/");
		assert!(dir.matches("a"));
		assert!(!dir.matches("aa"));
		assert!(!dir.matches("a/file"));
		let bare = ps(":(glob)a**");
		assert!(bare.matches("a") && bare.matches("aa") && bare.matches("a/file"));
		// A non-globstar dir-only glob (`:(glob)src*/`) still matches no file.
		assert!(!ps(":(glob)src*/").matches("src"));
		assert!(!ps(":(glob)src*/").matches("src/a"));
	}

	#[test]
	fn magic_only_pathspec_is_scoped_to_the_prefix() {
		// From a subdirectory, a non-`top` magic-only pathspec stays scoped to that directory (`:`,
		// `:(icase)`, `:(glob)` -> `sub/…`), while `:/` / `:(top)` reach the whole repo. Probed vs git.
		for spec in [":", ":(icase)", ":(glob)"] {
			let p = Pathspec::parse(spec, "sub").unwrap();
			assert!(p.matches("sub/a"), "{spec} should match under the prefix");
			assert!(!p.matches("other/c"), "{spec} must not escape the prefix");
			assert!(!p.matches("top"));
			assert!(
				p.expands_directory("sub/a"),
				"{spec} is a recursive dir expansion"
			);
		}
		for spec in [":/", ":(top)"] {
			let p = Pathspec::parse(spec, "sub").unwrap();
			assert!(p.matches("other/c"), "{spec} reaches the whole repo");
			assert!(p.matches("top"));
		}
	}

	#[test]
	fn literal_matches_exact_and_directory_contents() {
		let p = ps("src");
		assert!(p.matches("src")); // exact
		assert!(p.matches("src/a.rs")); // leading-directory contents
		assert!(!p.matches("srcx")); // not a `/` boundary
		assert!(!p.matches("other/src"));
	}

	#[test]
	fn glob_wildcards_cross_slash() {
		assert!(ps("*.rs").matches("src/a.rs"));
		assert!(ps("*.rs").matches("top.rs"));
		assert!(!ps("*.rs").matches("a.txt"));
		assert!(ps("src*").matches("src/sub/b.rs"));
	}

	#[test]
	fn glob_matching_a_directory_does_not_expand_contents() {
		// A glob that matches only the directory name does NOT pull in its contents (git behaviour,
		// probed vs git 2.50.1) — only a literal leading directory expands.
		assert!(!ps("sr?").matches("src/a.rs"));
		assert!(!ps("src/su?").matches("src/sub/b.rs"));
	}

	#[test]
	fn root_dot_matches_everything() {
		assert!(
			Pathspec::parse(".", "")
				.unwrap()
				.matches("anything/deep.rs")
		);
	}

	#[test]
	fn dir_only_requires_a_directory() {
		let p = ps("a.txt/"); // trailing slash
		assert!(!p.matches("a.txt")); // a file cannot satisfy a dir-only spec
		assert!(p.matches("a.txt/inner")); // matches as a directory
	}

	#[test]
	fn resolves_against_the_prefix() {
		// From a subdirectory, a glob is scoped to it (the prefix is prepended) and still crosses `/`.
		let p = Pathspec::parse("*.rs", "src").unwrap();
		assert!(p.matches("src/a.rs"));
		assert!(p.matches("src/sub/b.rs"));
		assert!(!p.matches("top.rs"));
	}

	#[test]
	fn magic_literal_disables_globbing() {
		// `:(literal)` treats a `*` as an ordinary character.
		let p = ps(":(literal)lit*name");
		assert!(p.matches("lit*name"));
		assert!(!p.matches("litXname"));
	}

	#[test]
	fn magic_glob_uses_pathname_mode() {
		// `:(glob)` is FNM_PATHNAME: `*` stops at `/`, `**` spans directories.
		assert!(!ps(":(glob)*.rs").matches("src/a.rs"));
		assert!(ps(":(glob)*.rs").matches("top.rs"));
		assert!(ps(":(glob)src/**/*.rs").matches("src/a.rs"));
		assert!(ps(":(glob)src/**/*.rs").matches("src/sub/b.rs"));
	}

	#[test]
	fn magic_icase_folds_literal_and_glob() {
		assert!(ps(":(icase)src/a.rs").matches("src/A.rs")); // literal exact, folded
		assert!(ps(":(icase)src").matches("SRC/a.rs")); // literal leading-dir, folded
		assert!(ps(":(icase)*.RS").matches("src/a.rs")); // glob, folded
	}

	#[test]
	fn magic_top_ignores_the_prefix() {
		// `:/` and `:(top)` resolve from the repo root even when invoked from a subdirectory.
		let p = Pathspec::parse(":/doc/*.md", "src").unwrap();
		assert!(p.matches("doc/x.md"));
		assert!(!p.matches("src/doc/x.md"));
		assert!(
			Pathspec::parse(":(top)a.txt", "sub")
				.unwrap()
				.matches("a.txt")
		);
	}

	#[test]
	fn magic_exclude_short_and_long_forms() {
		assert!(ps(":!vendor").is_exclude());
		assert!(ps(":^vendor").is_exclude());
		assert!(ps(":(exclude)vendor").is_exclude());
		assert!(!ps("vendor").is_exclude());
	}

	#[test]
	fn unknown_magic_is_rejected() {
		assert!(matches!(
			Pathspec::parse(":(bogus)x", ""),
			Err(WorktreeError::InvalidPathspecMagic(_))
		));
	}

	#[test]
	fn unsupported_short_magic_signature_is_rejected() {
		// Reserved-but-unimplemented short-magic signatures abort rather than becoming a literal path
		// (git 2.50.1: `:@x` -> "unimplemented magic"). A silent literal parse would let `rm ':@x'` remove
		// the file `@x`.
		for spec in [
			":@x", ":#x", ":-x", ":_x", ":~x", ":=x", ":,x", ":;x", ":<x", ":>x",
		] {
			assert!(
				matches!(
					Pathspec::parse(spec, ""),
					Err(WorktreeError::InvalidPathspecMagic(_))
				),
				"expected {spec} rejected"
			);
		}
		// Non-signature bytes still begin the path (the leading `:` is consumed): `:x`, `:.x`, `:*x`.
		assert_eq!(ps(":x").as_str(), "x");
		assert_eq!(ps(":.x").as_str(), ".x");
		assert!(ps(":*x").matches("ax")); // `*x` glob
		// Recognised signatures keep working.
		assert!(ps(":/x").matches("x")); // top-relative
		assert!(
			PathspecSet::parse(&[":!x"], "")
				.unwrap()
				.is_positive_empty()
		); // exclude
	}

	#[test]
	fn set_selects_positive_minus_negative() {
		let set = PathspecSet::parse(&[".", ":(exclude)vendor/*"], "").unwrap();
		assert!(set.matches("src/a.rs"));
		assert!(!set.matches("vendor/v.rs")); // excluded
		assert!(set.unmatched().is_none()); // the `.` positive matched
	}

	#[test]
	fn set_negative_only_selects_everything_not_excluded() {
		let set = PathspecSet::parse(&[":!vendor"], "").unwrap();
		assert!(set.is_positive_empty());
		assert!(!set.is_excluded("src/a.rs"));
		assert!(set.is_excluded("vendor/v.rs"));
	}

	#[test]
	fn set_reports_an_unmatched_positive() {
		let set = PathspecSet::parse(&["*.rs", "nomatch/*"], "").unwrap();
		assert!(set.matches("src/a.rs")); // `*.rs` matches; `nomatch/*` does not
		assert_eq!(set.unmatched(), Some("nomatch/*"));
	}

	#[test]
	fn glob_magic_on_a_literal_keeps_the_directory_rule() {
		// `:(glob)src` has no metacharacter, so it is literal and expands the `src` directory (git).
		let p = ps(":(glob)src");
		assert!(p.matches("src/a.rs"));
		assert!(p.matches("src"));
	}

	#[test]
	fn magic_only_root_matches_everything() {
		for spec in [":", ":/", ":(top)", ":(icase)"] {
			assert!(
				ps(spec).matches("any/deep/path.rs"),
				"{spec} should match the root"
			);
		}
	}

	#[test]
	fn unterminated_bracket_is_literal() {
		// `foo[` has no closing `]`, so it is an ordinary filename (git), not a character class.
		let p = ps("foo[");
		assert!(p.matches("foo["));
		assert!(!p.matches("fooX"));
	}

	#[test]
	fn incompatible_literal_glob_is_rejected() {
		assert!(matches!(
			Pathspec::parse(":(literal,glob)x", ""),
			Err(WorktreeError::InvalidPathspecMagic(_))
		));
	}

	#[test]
	fn dir_only_glob_matches_no_file() {
		// A directory-only glob matches no file entry (git reports it unmatched).
		let p = ps("src*/");
		assert!(!p.matches("srcfile"));
		assert!(!p.matches("src/b.rs"));
	}

	#[test]
	fn short_magic_colon_is_consumed() {
		// `:x` is path `x` with empty magic, not a literal `:x`; `::x` is the terminated equivalent (git).
		assert!(ps(":x").matches("x"));
		assert!(!ps(":x").matches(":x"));
		assert!(ps("::x").matches("x"));
	}

	#[test]
	fn backslash_escapes_a_literal_looking_spec() {
		// `foo\[` selects the file `foo[` — git honours the escape even without `:(glob)`.
		assert!(ps("foo\\[").matches("foo["));
		assert!(!ps("foo\\[").matches("fooX"));
	}

	#[test]
	fn empty_negated_class_matches_nothing() {
		// `foo[!]` / `foo[^]` are empty negated classes — git reports them unmatched.
		assert!(!ps("foo[!]").matches("fooX"));
		assert!(!ps("foo[^]").matches("fooX"));
	}

	#[test]
	fn expands_directory_marks_leading_dir_matches() {
		// A leading-directory expansion needs `-r`; an exact or glob file match does not.
		assert!(ps("foo").expands_directory("foo/bar"));
		assert!(!ps("foo").expands_directory("foo")); // exact
		assert!(ps(":(icase)FOO").expands_directory("foo/bar")); // folds case
		assert!(!ps("*.rs").expands_directory("a.rs")); // glob file match, not an expansion
		// A wildcard spec whose literal spelling names a directory expands it (`a?` -> `a?/f`).
		assert!(ps("a?").expands_directory("a?/f"));
		assert!(!ps("a?/f").expands_directory("ax/f")); // glob file match of `ax/f`, not an expansion
	}

	#[test]
	fn matches_unions_glob_and_literal_passes() {
		// A wildcard spec matches both its wildmatch (full path) and its literal spelling (exact or
		// leading directory): `a?` selects the literal directory `a?/`'s contents and the file `a?`, but
		// not `ax/f` (the 2-char glob cannot full-match a longer path). Probed vs git 2.50.1.
		assert!(ps("a?").matches("a?/f"));
		assert!(ps("a?").matches("a?"));
		assert!(ps("a?").matches("ax")); // glob full-match of the 2-char file
		assert!(!ps("a?").matches("ax/f"));
		// A pathspec ending in a dangling backslash matches only its literal spelling, never via glob.
		assert!(ps("?\\").matches("?\\"));
		assert!(!ps("?\\").matches("x\\"));
	}
}
