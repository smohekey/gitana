//! Include expansion for git config: `[include]` and `[includeIf "<cond>"]`.
//!
//! git expands includes *while parsing* — the included file's contents are spliced in at the
//! directive's position, so ordering and last-value-wins are preserved (see
//! [`GitConfigSource::expand_includes`](crate::GitConfigSource::expand_includes)). This crate is
//! I/O-free and wasm-pure, so the actual reads are delegated to a caller-supplied
//! [`IncludeResolver`], and the values needed to evaluate conditions (`$HOME`, the real gitdir, the
//! current branch) are supplied through an [`IncludeContext`].
//!
//! Conditions recognised: `gitdir:`/`gitdir/i:` (against the real gitdir), `onbranch:` (against the
//! short current branch), and `hasconfig:remote.*.url:` (against the remote URLs the driver supplies
//! in [`IncludeContext::remote_urls`]). Every other condition evaluates as non-matching, mirroring
//! git, which treats an unrecognised conditional as false.

use std::future::Future;
use std::path::{Path, PathBuf};

/// Reads the text of an include target, or `Ok(None)` if it is absent.
///
/// An absent include is silently skipped (git does not error on a missing `include.path`), which is
/// why "absent" is modelled as `Ok(None)` rather than an error. The read is async so both a native
/// driver (`tokio::fs`) and the wasm component (an async `FileStore` capability) can back it without
/// an API change; the crate itself performs no I/O. The future is `Send` (the desugared form
/// matching this crate's `FileStore` convention), and the trait is `Send + Sync` (like `FileStore`),
/// so an expansion — which holds `&resolver` across the read await — stays `Send` and spawnable.
pub trait IncludeResolver: Send + Sync {
	/// Return the file's text, or `None` if it does not exist.
	fn read(
		&self,
		path: &Path,
	) -> impl Future<Output = Result<Option<String>, crate::ConfigError>> + Send;
}

/// The ambient facts an [`IncludeResolver`]-driven expansion needs to resolve paths and evaluate
/// `includeIf` conditions. All fields are borrowed; a `None` means the corresponding fact is
/// unavailable, which makes any include that depends on it non-matching (never a match against an
/// absent fact).
pub struct IncludeContext<'a> {
	/// `$HOME`, used to expand a leading `~/` in an include path or a `gitdir:` pattern. `None`
	/// skips a `~/` include and makes a `~/` pattern non-matching.
	pub home: Option<&'a Path>,
	/// The real (driver-resolved, absolute) gitdir, matched by `gitdir:`/`gitdir/i:` conditions.
	/// `None` makes every `gitdir:` condition non-matching.
	pub gitdir: Option<&'a Path>,
	/// The short branch name of a **symbolic** `HEAD` (its `refs/heads/<name>` target, sans prefix),
	/// matched by `onbranch:`. git reads it straight off HEAD's symref, so it is present whenever HEAD
	/// is symbolic — **including an unborn branch and a bare repository** (probed 2.50.1: a bare repo
	/// with `HEAD -> refs/heads/main` matches `onbranch:main`). `None` is only a **detached** (or
	/// otherwise unresolvable) HEAD, which makes every `onbranch:` condition non-matching, as in git.
	pub branch: Option<&'a str>,
	/// Every `remote.<name>.url` value across the *whole* effective config — all precedence layers and
	/// all includes, both before and after the directive — matched by `hasconfig:remote.*.url:`. This
	/// is not "config so far": git resolves the condition by scanning the entire config for remote
	/// URLs (a separate pre-scan pass), so the *driver*, which alone sees every layer, collects the
	/// list; the engine only wildmatches the condition's value-glob against it. `None` or empty makes
	/// every `hasconfig:` condition non-matching.
	pub remote_urls: Option<&'a [&'a str]>,
}

/// Resolve a *matched* `include.path` / `includeif.path` value to a filesystem path.
///
/// A leading `~/` — or a bare `~` on its own — expands against `home` (git fatals —
/// [`ConfigError::IncludeTildeNoHome`] — when `home` is `None`, distinct from an absent target file,
/// which the caller skips); a `~user/` form needs a passwd lookup this pure crate cannot do, so it
/// fails closed ([`ConfigError::IncludeUserTildeUnsupported`], a native-driver follow-up) rather than
/// being mis-read as a relative path; a relative path is joined onto `dir` (the including file's
/// directory); an absolute path is taken as-is.
pub(crate) fn resolve_include_path(
	value: &str,
	dir: &Path,
	home: Option<&Path>,
) -> Result<PathBuf, crate::ConfigError> {
	if value == "~" {
		// A bare `~` is `$HOME` itself (git expands it before the `~user` interpretation).
		Ok(
			home
				.ok_or(crate::ConfigError::IncludeTildeNoHome)?
				.to_path_buf(),
		)
	} else if let Some(rest) = value.strip_prefix("~/") {
		let home = home.ok_or(crate::ConfigError::IncludeTildeNoHome)?;
		// Strip leading separators from the remainder so `~//x` does not reset the base to the
		// filesystem root (`Path::join` treats an absolute right-hand side as replacing the left).
		Ok(home.join(rest.trim_start_matches('/')))
	} else if value.starts_with('~') {
		Err(crate::ConfigError::IncludeUserTildeUnsupported)
	} else {
		let path = Path::new(value);
		if path.is_absolute() {
			Ok(path.to_path_buf())
		} else {
			Ok(dir.join(path))
		}
	}
}

/// Evaluate an `includeIf` condition string (the header's subsection, verbatim), e.g.
/// `gitdir:~/work/`, `onbranch:feature/*`, or `hasconfig:remote.*.url:https://host/**`. Returns
/// whether the include should be applied.
///
/// Recognised conditions: `gitdir:` (case-sensitive), `gitdir/i:` (case-insensitive) against the real
/// gitdir; `onbranch:` against the short current branch; and — matching git, which special-cases only
/// this one form — the *literal* prefix `hasconfig:remote.*.url:` against the driver-supplied remote
/// URLs. Every other condition (including a general `hasconfig:<var>:<value>`, which git does **not**
/// implement) is non-matching, mirroring git's treatment of an unrecognised conditional as false.
pub(crate) fn condition_matches(condition: &str, dir: &Path, ctx: &IncludeContext<'_>) -> bool {
	if let Some(pattern) = condition.strip_prefix("gitdir:") {
		gitdir_matches(pattern, dir, ctx, false)
	} else if let Some(pattern) = condition.strip_prefix("gitdir/i:") {
		gitdir_matches(pattern, dir, ctx, true)
	} else if let Some(pattern) = condition.strip_prefix("onbranch:") {
		onbranch_matches(pattern, ctx)
	} else if let Some(glob) = condition.strip_prefix(HASCONFIG_REMOTE_URL_PREFIX) {
		hasconfig_remote_url_matches(glob, ctx)
	} else {
		false
	}
}

/// The one `hasconfig:` form git implements: `remote.*.url` is hardcoded, matched as a *literal*
/// prefix (not a general `<var-glob>`), and everything after it is the value-glob applied to the
/// config's remote URLs. A `hasconfig:some.other.key:…` or a wildcarded var (`remote.?.url:`) is
/// therefore unrecognised and non-matching, exactly as in git.
const HASCONFIG_REMOTE_URL_PREFIX: &str = "hasconfig:remote.*.url:";

/// Whether an `includeIf "hasconfig:remote.*.url:…"` directive is what `condition` is, so the caller
/// can enforce git's paradox guard (a file it includes may not itself set a remote URL).
pub(crate) fn is_hasconfig_remote_url(condition: &str) -> bool {
	condition.starts_with(HASCONFIG_REMOTE_URL_PREFIX)
}

/// Match an `onbranch:<pat>` condition against `ctx.branch` (the short current branch).
///
/// git's `onbranch` preprocessing is *only* the trailing-`/`→`**` rule (unlike `gitdir`, it does
/// **not** prepend `**/`, so `onbranch:foo` does not match branch `feature/foo`); the result is a
/// case-sensitive `WM_PATHNAME` wildmatch. A `None` branch (a detached HEAD) never matches.
fn onbranch_matches(pattern: &str, ctx: &IncludeContext<'_>) -> bool {
	let Some(branch) = ctx.branch else {
		return false;
	};
	let pattern = append_trailing_starstar(pattern.to_owned());
	wildmatch(pattern.as_bytes(), branch.as_bytes(), false)
}

/// Match a `hasconfig:remote.*.url:<glob>` condition: true when *any* remote URL the driver supplied
/// matches `glob` under a plain, anchored, case-sensitive `WM_PATHNAME` wildmatch (git applies no
/// prefix/suffix preprocessing to this value-glob). An absent or empty URL list never matches.
fn hasconfig_remote_url_matches(glob: &str, ctx: &IncludeContext<'_>) -> bool {
	let Some(urls) = ctx.remote_urls else {
		return false;
	};
	urls
		.iter()
		.any(|url| wildmatch(glob.as_bytes(), url.as_bytes(), false))
}

/// git's trailing-directory rule shared by `gitdir:` and `onbranch:`: a pattern ending in `/` gains a
/// `**`, so pointing at a parent directory (or a branch namespace) matches everything under it.
fn append_trailing_starstar(mut pattern: String) -> String {
	if pattern.ends_with('/') {
		pattern.push_str("**");
	}
	pattern
}

/// Match a `gitdir[/i]:` pattern against `ctx.gitdir`.
///
/// gitdir matching assumes git's slash-form (`/`-separated) paths, as the Unix/wasm drivers supply.
/// Windows path normalization (backslash separators, verbatim `\\?\` prefixes) is a deferred
/// follow-up.
fn gitdir_matches(pattern: &str, dir: &Path, ctx: &IncludeContext<'_>, icase: bool) -> bool {
	let Some(gitdir) = ctx.gitdir else {
		return false;
	};
	let Some((pattern, prefix)) = prepare_gitdir_pattern(pattern, dir, ctx.home) else {
		return false;
	};

	let text = path_to_string(gitdir);
	let pattern = pattern.as_bytes();
	let text = text.as_bytes();

	// git matches the interpolated `./<dir>` prefix literally (so a wildcard within it cannot cross
	// a `/`), running the wildmatch only on the remainder.
	if prefix > 0 && (text.len() < prefix || !bytes_eq(&pattern[..prefix], &text[..prefix], icase)) {
		return false;
	}
	wildmatch(&pattern[prefix..], &text[prefix..], icase)
}

/// Apply git's `prepare_include_condition_pattern` preprocessing, returning the effective pattern
/// and the length of any leading literal (non-wildmatch) prefix. Returns `None` when the pattern
/// cannot be expanded (a `~`/`~/` with no `home`, or an unsupported `~user/`).
fn prepare_gitdir_pattern(
	pattern: &str,
	dir: &Path,
	home: Option<&Path>,
) -> Option<(String, usize)> {
	let mut prefix = 0;

	// Interpolate a leading `~/` (or a bare `~`) against home — matching the include-path tilde rule.
	let mut pattern = if pattern == "~" {
		path_to_string(home?)
	} else if let Some(rest) = pattern.strip_prefix("~/") {
		let mut home = path_to_string(home?);
		if !home.ends_with('/') {
			home.push('/');
		}
		home.push_str(rest);
		home
	} else if pattern.starts_with('~') {
		return None;
	} else {
		pattern.to_owned()
	};

	if let Some(rest) = pattern.strip_prefix("./") {
		// `./` is relative to the including file's directory; that directory is matched literally.
		// NOTE (deferred, slice 3): `dir` is used lexically here — a file reached via a `..`/symlinked
		// path can have a lexical parent that differs from its realpath, which git resolves. The pure
		// crate cannot realpath (no fs), so the driver must pass a canonicalized including-file dir.
		let mut base = path_to_string(dir);
		if !base.ends_with('/') {
			base.push('/');
		}
		prefix = base.len();
		pattern = format!("{base}{rest}");
	} else if !pattern.starts_with('/') {
		// Any other non-absolute pattern is anchored with `**/` (so `foo` becomes `**/foo`), matching
		// git — which prepends `**/` to every relative, non-`./` pattern, not only slash-free ones.
		pattern = format!("**/{pattern}");
	}

	// A trailing `/` appends `**`, so pointing at a parent directory matches everything under it.
	let pattern = append_trailing_starstar(pattern);

	Some((pattern, prefix))
}

/// git's `wildmatch` with `WM_PATHNAME` semantics, over bytes.
///
/// `?` matches one non-`/` byte; a single `*` matches any run of non-`/` bytes (staying within a
/// path segment); a `**` that forms a whole path segment (bounded by the start/end of the pattern
/// or a `/` on each side) matches any number of segments, including none; `[...]` is a bracket
/// expression (ranges `a-z`, sets, and `[!…]`/`[^…]` negation) matching one non-`/` byte; a
/// backslash escapes the next byte to a literal; every other byte is literal. `icase` folds ASCII
/// case. Anchored at both ends (the whole `text` must be consumed). A malformed construct — an
/// unterminated `[`, a terminated-but-unknown POSIX class `[[:bogus:]]`, or a trailing backslash —
/// aborts the whole match (git's `WM_ABORT_ALL`), so the pattern matches nothing. (A `[:` with no
/// `:]` terminator is *not* malformed: git, and [`parse_class`], treat the `[` as an ordinary set
/// member — see [`tokenize`]/[`parse_class`].)
///
/// The stars are the only source of non-determinism: every other token matches exactly one byte, so
/// the pattern's leading run (before the first star) and trailing run (after the last star) are
/// anchored to the text's ends and matched in **linear lockstep**. Only the between-stars middle needs
/// the dynamic program. This keeps the common case — a star-free pattern, or one star surrounded by
/// literals (e.g. a `hasconfig` URL glob) — **O(text)**, matching git, while an adversarial *multi*-star
/// middle (`*a*a…b`) stays **polynomial** (git's own `wildmatch` is exponential there). No length cap is
/// imposed: git matches long patterns, and capping would reject inputs git accepts.
fn wildmatch(pattern: &[u8], text: &[u8], icase: bool) -> bool {
	let tokens = tokenize(pattern);
	let n = text.len();

	// Anchor the leading non-star run to the front. Each such token consumes exactly one byte.
	let first_star = tokens.iter().position(is_star).unwrap_or(tokens.len());
	let mut lo = 0;
	for token in &tokens[..first_star] {
		if lo >= n || !match_one(token, text[lo], icase) {
			return false;
		}
		lo += 1;
	}
	// No stars: the pattern is fully anchored, so the text must be exactly consumed.
	if first_star == tokens.len() {
		return lo == n;
	}

	// Anchor the trailing non-star run to the back (there is at least one star, so this run is disjoint
	// from the leading one in the pattern; a shortfall of text is caught by the `hi <= lo` guard).
	let last_star = tokens.iter().rposition(is_star).unwrap();
	let mut hi = n;
	for token in tokens[last_star + 1..].iter().rev() {
		if hi <= lo || !match_one(token, text[hi - 1], icase) {
			return false;
		}
		hi -= 1;
	}

	// Only the middle (first star through last star, inclusive) is non-deterministic.
	dp_match(&tokens[first_star..=last_star], &text[lo..hi], icase)
}

/// Whether `token` is a `*`/`**` (the only tokens that match a variable-length run).
fn is_star(token: &Token) -> bool {
	matches!(token, Token::Star(_))
}

/// Whether a single, exactly-one-byte token (`Literal`/`?`/`[...]`) matches `byte`. A `Token::Star`
/// must not be passed here; a `Token::NeverMatch` never matches (git's `WM_ABORT_ALL`).
fn match_one(token: &Token, byte: u8, icase: bool) -> bool {
	match token {
		Token::NeverMatch => false,
		Token::Literal(b) => byte_eq(byte, *b, icase),
		Token::Any => byte != b'/',
		Token::Class(class) => byte != b'/' && class.matches(byte, icase),
		Token::Star(_) => unreachable!("match_one is only called on single-byte tokens"),
	}
}

/// Anchored `WM_PATHNAME` match of a token slice that begins and ends with a `*`/`**`, over bytes, as
/// an **O(tokens × text) dynamic program** (not recursive backtracking, which is exponential on an
/// adversarial `*a*a…b`). Only the previous DP row is kept, so space is **O(text)** — a large
/// attacker-controlled glob/URL cannot exhaust memory with a full matrix.
fn dp_match(tokens: &[Token], text: &[u8], icase: bool) -> bool {
	let k = tokens.len();
	let n = text.len();

	// `next_slash[j]` = smallest index m >= j with text[m] == b'/', else n. Lets the segment star
	// jump a whole path component at a time.
	let mut next_slash = vec![n; n + 1];
	for j in (0..n).rev() {
		next_slash[j] = if text[j] == b'/' {
			j
		} else {
			next_slash[j + 1]
		};
	}

	// `dp[i][j]` = does `tokens[i..]` match `text[j..]`. Row `i` reads only row `i + 1` and its own
	// already-computed columns (`j` descends), so two rolling rows suffice: `next` holds row `i + 1`,
	// `cur` the row `i` being filled. The base row `i == k` (no tokens left) matches only the empty
	// tail: `dp[k][j] = (j == n)`.
	let mut next: Vec<bool> = (0..=n).map(|j| j == n).collect();
	let mut cur = vec![false; n + 1];
	for i in (0..k).rev() {
		for j in (0..=n).rev() {
			cur[j] = match &tokens[i] {
				// A malformed construct makes the whole pattern unsatisfiable, as git's WM_ABORT_ALL.
				Token::NeverMatch => false,
				Token::Literal(b) => j < n && byte_eq(text[j], *b, icase) && next[j + 1],
				Token::Any => j < n && text[j] != b'/' && next[j + 1],
				Token::Class(class) => {
					j < n && text[j] != b'/' && class.matches(text[j], icase) && next[j + 1]
				}
				// A single `*` matches empty, or one more non-`/` byte within the same segment.
				Token::Star(StarKind::Single) => next[j] || (j < n && text[j] != b'/' && cur[j + 1]),
				// A trailing `**` matches any remainder, crossing `/`.
				Token::Star(StarKind::Trailing) => next[j] || (j < n && cur[j + 1]),
				// A `**/` matches zero or more *complete* path components: either nothing (resume at
				// this boundary), or up to and including the next `/` then recurse at that boundary.
				Token::Star(StarKind::Segment) => {
					let s = next_slash[j];
					next[j] || (s < n && (next[s + 1] || cur[s + 1]))
				}
			};
		}
		// Row `i` becomes row `i + 1` for the next (lower) token index.
		std::mem::swap(&mut next, &mut cur);
	}
	// The final swap left row `0` in `next`.
	next[0]
}

/// How a collapsed `*`/`**` run matches (see [`wildmatch`]).
enum StarKind {
	/// Single-segment `*` (or a non-boundary `**`): never crosses `/`.
	Single,
	/// A whole-segment `**` at the end of the pattern: crosses `/`, matches the rest.
	Trailing,
	/// A whole-segment `**` followed by `/` (the `/` is folded in): matches complete components.
	Segment,
}

/// One wildmatch token; a `*`/`**` run and a `[...]` class each collapse to a single token.
enum Token {
	Literal(u8),
	/// `?`
	Any,
	Star(StarKind),
	Class(Class),
	/// A malformed construct (an unterminated `[`): the whole pattern can never match, as in git.
	NeverMatch,
}

/// A parsed `[...]` bracket expression.
struct Class {
	negated: bool,
	items: Vec<ClassItem>,
}

enum ClassItem {
	Char(u8),
	Range(u8, u8),
	/// A POSIX character class, `[:name:]`.
	Posix(PosixClass),
}

/// The POSIX character classes git's wildmatch recognizes inside a `[...]` (e.g. `[[:digit:]]`).
#[derive(Clone, Copy)]
enum PosixClass {
	Alnum,
	Alpha,
	Blank,
	Cntrl,
	Digit,
	Graph,
	Lower,
	Print,
	Punct,
	Space,
	Upper,
	Xdigit,
}

impl PosixClass {
	/// The class named `name` (the text between `[:` and `:]`), or `None` if unrecognized.
	fn from_name(name: &[u8]) -> Option<Self> {
		Some(match name {
			b"alnum" => Self::Alnum,
			b"alpha" => Self::Alpha,
			b"blank" => Self::Blank,
			b"cntrl" => Self::Cntrl,
			b"digit" => Self::Digit,
			b"graph" => Self::Graph,
			b"lower" => Self::Lower,
			b"print" => Self::Print,
			b"punct" => Self::Punct,
			b"space" => Self::Space,
			b"upper" => Self::Upper,
			b"xdigit" => Self::Xdigit,
			_ => return None,
		})
	}

	/// Whether the (ASCII) byte `c` is a member. Under `icase`, `[:lower:]`/`[:upper:]` fold so either
	/// case matches (the other classes are case-agnostic).
	fn matches(self, c: u8, icase: bool) -> bool {
		match self {
			Self::Alnum => c.is_ascii_alphanumeric(),
			Self::Alpha => c.is_ascii_alphabetic(),
			Self::Blank => c == b' ' || c == b'\t',
			Self::Cntrl => c.is_ascii_control(),
			Self::Digit => c.is_ascii_digit(),
			Self::Graph => c.is_ascii_graphic(),
			// `is_ascii_whitespace` omits the vertical tab that POSIX `[:space:]` includes.
			Self::Space => c.is_ascii_whitespace() || c == 0x0b,
			Self::Print => c.is_ascii_graphic() || c == b' ',
			Self::Punct => c.is_ascii_punctuation(),
			Self::Lower => c.is_ascii_lowercase() || (icase && c.is_ascii_uppercase()),
			Self::Upper => c.is_ascii_uppercase() || (icase && c.is_ascii_lowercase()),
			Self::Xdigit => c.is_ascii_hexdigit(),
		}
	}
}

impl Class {
	/// Whether `c` is a member (after negation). The caller has already excluded `/`.
	fn matches(&self, c: u8, icase: bool) -> bool {
		let hit = self.items.iter().any(|item| match *item {
			ClassItem::Char(b) => byte_eq(c, b, icase),
			// git reads a range's low endpoint as an ordinary member *before* it sees the `-`, so `lo`
			// matches as a literal in addition to the `lo..=hi` span. This only shows for a descending
			// range (`[b-a]` matches `b`), where the span is empty; for an ascending range `lo` is
			// already in the span.
			ClassItem::Range(lo, hi) => byte_eq(c, lo, icase) || in_range(c, lo, hi, icase),
			ClassItem::Posix(class) => class.matches(c, icase),
		});
		hit != self.negated
	}
}

/// Split a pattern into [`Token`]s, collapsing `*`/`**` runs and `[...]` classes.
fn tokenize(pattern: &[u8]) -> Vec<Token> {
	let mut tokens = Vec::new();
	let mut i = 0;
	while i < pattern.len() {
		match pattern[i] {
			b'*' => {
				let start = i;
				while i < pattern.len() && pattern[i] == b'*' {
					i += 1;
				}
				let double = i - start >= 2;
				// A `**` crosses `/` only as a whole path segment (a `/` — or start/end — on each
				// side). An *escaped* separator (`\/`) after `**` is not treated as a segment boundary
				// here; that exotic case is a deferred follow-up.
				let prev_boundary = start == 0 || pattern[start - 1] == b'/';
				let next_boundary = i >= pattern.len() || pattern[i] == b'/';
				if double && prev_boundary && next_boundary {
					if i < pattern.len() {
						// `**/` — fold the trailing `/` into the segment star.
						tokens.push(Token::Star(StarKind::Segment));
						i += 1;
					} else {
						tokens.push(Token::Star(StarKind::Trailing));
					}
				} else {
					tokens.push(Token::Star(StarKind::Single));
				}
			}
			b'?' => {
				tokens.push(Token::Any);
				i += 1;
			}
			b'[' => match parse_class(pattern, i) {
				Some((class, end)) => {
					tokens.push(Token::Class(class));
					i = end;
				}
				// An unterminated `[` makes the whole pattern non-matching, as git's wildmatch does
				// (it aborts the match rather than treating the `[` as a literal).
				None => {
					tokens.push(Token::NeverMatch);
					break;
				}
			},
			b'\\' => {
				if i + 1 < pattern.len() {
					tokens.push(Token::Literal(pattern[i + 1]));
					i += 2;
				} else {
					// A trailing backslash (nothing to escape) aborts the whole match, as git's
					// wildmatch does (`WM_ABORT_ALL`) rather than matching a literal `\`.
					tokens.push(Token::NeverMatch);
					break;
				}
			}
			other => {
				tokens.push(Token::Literal(other));
				i += 1;
			}
		}
	}
	tokens
}

/// Parse a `[...]` class beginning at `start` (`pattern[start] == b'['`), returning the class and
/// the index just past the closing `]`, or `None` if the class is malformed (unterminated, or a bad
/// POSIX `[:...` — git aborts the whole match in both cases, which the caller renders as a
/// [`Token::NeverMatch`]).
fn parse_class(pattern: &[u8], start: usize) -> Option<(Class, usize)> {
	let mut i = start + 1;
	let mut negated = false;
	if matches!(pattern.get(i), Some(b'!' | b'^')) {
		negated = true;
		i += 1;
	}
	let mut items = Vec::new();
	let mut first = true;
	while i < pattern.len() {
		// A `]` closes the class, except as the very first member (where it is a literal `]`).
		if pattern[i] == b']' && !first {
			return Some((Class { negated, items }, i + 1));
		}
		first = false;
		// A `[:name:]` POSIX class. git recognises it only when the run from `[:` to the next `]` is
		// terminated `…:]` (the `]` immediately preceded by `:`) with a non-empty name; it then aborts
		// the whole match on an unknown name (git's `WM_ABORT_ALL`). Otherwise the `[` is just an
		// ordinary set member and parsing continues from the `:` — so e.g. `[[:abc]` is the set
		// `{[ : a b c}`, matching a literal `[`, `:`, `a`, `b`, or `c`. (Mirrors git's `dowild`.)
		if pattern[i] == b'[' && pattern.get(i + 1) == Some(&b':') {
			let name_start = i + 2;
			let mut close = name_start;
			while close < pattern.len() && pattern[close] != b']' {
				close += 1;
			}
			if close >= pattern.len() {
				// No `]` at all: the whole bracket is unterminated, which git aborts.
				return None;
			}
			if close > name_start && pattern[close - 1] == b':' {
				// Terminated `[:name:]`: a known name is the class; an unknown one aborts the match.
				let name = &pattern[name_start..close - 1];
				match PosixClass::from_name(name) {
					Some(class) => {
						items.push(ClassItem::Posix(class));
						i = close + 1; // skip past the closing `]`
						continue;
					}
					None => return None,
				}
			}
			// Not a `…:]` terminator: treat the `[` as an ordinary member and resume from the `:`.
			items.push(ClassItem::Char(b'['));
			i += 1;
			continue;
		}
		// A member is an escaped byte, a range `lo-hi`, or a single byte.
		let (lo, after_lo) = if pattern[i] == b'\\' && i + 1 < pattern.len() {
			(pattern[i + 1], i + 2)
		} else {
			(pattern[i], i + 1)
		};
		if pattern.get(after_lo) == Some(&b'-')
			&& after_lo + 1 < pattern.len()
			&& pattern[after_lo + 1] != b']'
		{
			let hi_at = after_lo + 1;
			let (hi, after_hi) = if pattern[hi_at] == b'\\' && hi_at + 1 < pattern.len() {
				(pattern[hi_at + 1], hi_at + 2)
			} else {
				(pattern[hi_at], hi_at + 1)
			};
			items.push(ClassItem::Range(lo, hi));
			i = after_hi;
		} else {
			items.push(ClassItem::Char(lo));
			i = after_lo;
		}
	}
	None
}

/// Whether `c` lies in the `lo..=hi` byte range, folding ASCII case when `icase`.
fn in_range(c: u8, lo: u8, hi: u8, icase: bool) -> bool {
	if (lo..=hi).contains(&c) {
		return true;
	}
	if icase {
		let (ll, lh) = (lo.to_ascii_lowercase(), hi.to_ascii_lowercase());
		let (ul, uh) = (lo.to_ascii_uppercase(), hi.to_ascii_uppercase());
		let cl = c.to_ascii_lowercase();
		let cu = c.to_ascii_uppercase();
		return (ll..=lh).contains(&cl)
			|| (ll..=lh).contains(&cu)
			|| (ul..=uh).contains(&cl)
			|| (ul..=uh).contains(&cu);
	}
	false
}

/// Compare two bytes, folding ASCII case when `icase`.
fn byte_eq(a: u8, b: u8, icase: bool) -> bool {
	if icase {
		a.eq_ignore_ascii_case(&b)
	} else {
		a == b
	}
}

/// Compare two byte slices, folding ASCII case when `icase`.
fn bytes_eq(a: &[u8], b: &[u8], icase: bool) -> bool {
	a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| byte_eq(x, y, icase))
}

/// Render a path as a string for pattern matching. Non-UTF-8 native paths degrade lossily here;
/// slice 1 documents that the driver supplies real, UTF-8 gitdir/home paths (see the HLD).
fn path_to_string(path: &Path) -> String {
	path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;
	use crate::{ConfigError, GitConfigSource};

	/// An in-memory [`IncludeResolver`] backed by a path→text map; an unmapped path reads as absent.
	#[derive(Default)]
	struct MapResolver {
		files: HashMap<PathBuf, String>,
	}

	impl MapResolver {
		fn with(mut self, path: &str, text: &str) -> Self {
			self.files.insert(PathBuf::from(path), text.to_owned());
			self
		}
	}

	impl IncludeResolver for MapResolver {
		async fn read(&self, path: &Path) -> Result<Option<String>, ConfigError> {
			Ok(self.files.get(path).cloned())
		}
	}

	fn ctx<'a>(home: Option<&'a Path>, gitdir: Option<&'a Path>) -> IncludeContext<'a> {
		IncludeContext {
			home,
			gitdir,
			branch: None,
			remote_urls: None,
		}
	}

	/// Parse `source`, expand its includes from `dir`, and return the expanded in-memory config. Tests
	/// read values off it directly (the read getters see own **and** included elements); they must NOT
	/// round-trip through `render()`, which intentionally emits own-only content.
	fn expand(
		source: &str,
		dir: &str,
		ctx: &IncludeContext<'_>,
		resolver: &MapResolver,
	) -> GitConfigSource {
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new(dir), ctx, resolver)).unwrap();
		config
	}

	// --- expand_includes: ordering, path resolution, recursion, missing files ---

	#[test]
	fn include_splices_inline_preserving_last_value_wins() {
		// A value before the include is overridden by it; a value after the include overrides it.
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[user]\n\tname = from-include\n");
		let source = concat!(
			"[user]\n",
			"\tname = before\n",
			"[include]\n\tpath = inc.cfg\n",
			"[user]\n\tname = after\n",
		);
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(config.get_string("user", None, "name"), Some("after"));
		// git keeps the directive queryable as an ordinary key WHILE also expanding its content.
		assert_eq!(config.get_string("include", None, "path"), Some("inc.cfg"));

		// With no later override, the include's value wins over the earlier one.
		let source = concat!("[user]\n\tname = before\n", "[include]\n\tpath = inc.cfg\n",);
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(
			config.get_string("user", None, "name"),
			Some("from-include")
		);
		assert_eq!(config.get_string("include", None, "path"), Some("inc.cfg"));
	}

	#[test]
	fn relative_absolute_and_tilde_paths_resolve() {
		let home = PathBuf::from("/home/me");
		let resolver = MapResolver::default()
			.with("/etc/rel.cfg", "[core]\n\trel = 1\n")
			.with("/abs/here.cfg", "[core]\n\tabs = 1\n")
			.with("/home/me/tilde.cfg", "[core]\n\ttilde = 1\n");
		let source = concat!(
			"[include]\n\tpath = rel.cfg\n",
			"[include]\n\tpath = /abs/here.cfg\n",
			"[include]\n\tpath = ~/tilde.cfg\n",
		);
		let config = expand(source, "/etc", &ctx(Some(home.as_path()), None), &resolver);
		assert_eq!(config.get_string("core", None, "rel"), Some("1"));
		assert_eq!(config.get_string("core", None, "abs"), Some("1"));
		assert_eq!(config.get_string("core", None, "tilde"), Some("1"));
	}

	#[test]
	fn tilde_include_without_home_is_fatal() {
		// A MATCHED `~/` include with no `$HOME` is fatal in git (`could not expand include path`),
		// distinct from an absent target file (which is skipped).
		let resolver = MapResolver::default().with("/home/me/x.cfg", "[core]\n\tx = 1\n");
		let mut config = GitConfigSource::parse("[include]\n\tpath = ~/x.cfg\n").unwrap();
		let err =
			block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap_err();
		assert!(matches!(err, ConfigError::IncludeTildeNoHome), "{err:?}");
	}

	#[test]
	fn user_tilde_include_is_unsupported_and_fatal() {
		// `~user/` needs a passwd lookup this pure crate cannot do: fail closed rather than mis-read it
		// as a relative path.
		let resolver = MapResolver::default();
		let mut config = GitConfigSource::parse("[include]\n\tpath = ~alice/x.cfg\n").unwrap();
		let err =
			block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap_err();
		assert!(
			matches!(err, ConfigError::IncludeUserTildeUnsupported),
			"{err:?}"
		);
	}

	#[test]
	fn doubled_slash_tilde_does_not_discard_home() {
		// `~//inc.cfg` must expand under `$HOME`, not reset to the filesystem root.
		let home = PathBuf::from("/home/me");
		let resolver = MapResolver::default().with("/home/me/inc.cfg", "[core]\n\tx = 1\n");
		let source = "[include]\n\tpath = ~//inc.cfg\n";
		let config = expand(source, "/etc", &ctx(Some(home.as_path()), None), &resolver);
		assert_eq!(config.get_string("core", None, "x"), Some("1"));
	}

	#[test]
	fn bare_tilde_include_path_resolves_to_home() {
		// `path = ~` (exactly) is `$HOME` itself — the home directory is the include target file.
		let home = PathBuf::from("/home/me");
		let resolver = MapResolver::default().with("/home/me", "[core]\n\tx = 1\n");
		let source = "[include]\n\tpath = ~\n";
		let config = expand(source, "/etc", &ctx(Some(home.as_path()), None), &resolver);
		assert_eq!(config.get_string("core", None, "x"), Some("1"));
	}

	#[test]
	fn render_keeps_the_directive_and_omits_flattened_included_content() {
		// git keeps `[include] path=…` on disk and never flattens the included file into it.
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[user]\n\temail = incl@x\n");
		let source = "[user]\n\tname = me\n[include]\n\tpath = inc.cfg\n";
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();
		// Reads see the included value...
		assert_eq!(config.get_string("user", None, "email"), Some("incl@x"));
		// ...but render() emits only the own content: the directive stays, the included body does not.
		let rendered = config.render();
		assert!(
			rendered.contains("path = inc.cfg"),
			"rendered: {rendered:?}"
		);
		assert!(!rendered.contains("incl@x"), "flattened: {rendered:?}");
		assert_eq!(rendered, source);
	}

	#[test]
	fn writes_touch_own_elements_only_leaving_included_values_alone() {
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[user]\n\temail = incl@x\n");
		let source = "[user]\n\tname = me\n[include]\n\tpath = inc.cfg\n";
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();

		// `set` of a key that lives only in the include inserts a NEW own line into the own section; it
		// never edits the included occurrence, so render carries the own value and not the included one.
		config.set("user", None, "email", "own@x").unwrap();
		let rendered = config.render();
		assert!(rendered.contains("email = own@x"), "rendered: {rendered:?}");
		assert!(!rendered.contains("incl@x"), "leaked include: {rendered:?}");
		assert!(
			rendered.contains("path = inc.cfg"),
			"directive dropped: {rendered:?}"
		);

		// `unset` of an own key removes only the own line; the included value stays visible to reads.
		assert!(config.unset("user", None, "name"));
		assert!(config.get_string("user", None, "name").is_none());
		assert_eq!(config.get_string("user", None, "email"), Some("incl@x"));
		// A second unset of the (now own-absent) email cannot remove the read-only included value.
		assert!(config.unset("user", None, "email"));
		assert_eq!(config.get_string("user", None, "email"), Some("incl@x"));
	}

	#[test]
	fn set_new_section_after_a_newlineless_include_lands_on_own_text() {
		// The final OWN line is an include directive with no trailing newline, and a matched include
		// splices Included entries after it. A `set` into a brand-new section must add its separating
		// newline to that own directive line (render drops the Included ones), not concatenate.
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[user]\n\temail = incl@x\n");
		let source = "[core]\n\tbare = false\n[include]\n\tpath = inc.cfg";
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();

		config.set("new", None, "key", "v").unwrap();
		let rendered = config.render();
		assert!(
			!rendered.contains("inc.cfg[new]"),
			"concatenated: {rendered:?}"
		);
		// Re-parsing proves the key is filed under `[new]`, not glued onto `[include]`.
		let reparsed = GitConfigSource::parse(&rendered).unwrap();
		assert_eq!(reparsed.get_string("new", None, "key"), Some("v"));
		assert_eq!(
			reparsed.get_string("include", None, "path"),
			Some("inc.cfg")
		);
	}

	#[test]
	fn expand_includes_is_idempotent_and_reflects_target_changes() {
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[core]\n\tk = one\n");
		let source = "[include]\n\tpath = inc.cfg\n";
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();
		// Re-expanding must not duplicate the included values (the prior Included entries are dropped).
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();
		assert_eq!(config.get_all("core", None, "k"), vec!["one"]);

		// A changed target, re-expanded, replaces the old value — no stale value survives to win.
		let changed = MapResolver::default().with("/etc/inc.cfg", "[core]\n\tk = two\n");
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &changed)).unwrap();
		assert_eq!(config.get_all("core", None, "k"), vec!["two"]);
		assert_eq!(config.get_string("core", None, "k"), Some("two"));
	}

	#[test]
	fn missing_include_file_is_silently_skipped() {
		let resolver = MapResolver::default();
		let source = "[user]\n\tname = a\n[include]\n\tpath = nope.cfg\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(config.get_string("user", None, "name"), Some("a"));
		// A missing target is skipped, but git still keeps the directive queryable.
		assert_eq!(config.get_string("include", None, "path"), Some("nope.cfg"));
	}

	#[test]
	fn subsectioned_include_is_an_ordinary_key_not_a_directive() {
		// `[include "profile"] path = X` is the ordinary key `include.profile.path`; git does not read
		// X (only a bare `[include]` is special).
		let resolver = MapResolver::default().with("/etc/extra.cfg", "[user]\n\temail = x\n");
		let source = "[include \"profile\"]\n\tpath = extra.cfg\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		// Not expanded.
		assert!(config.get_string("user", None, "email").is_none());
		// The subsectioned key survives as a normal value.
		assert_eq!(
			config.get_string("include", Some("profile"), "path"),
			Some("extra.cfg")
		);
	}

	#[test]
	fn render_round_trips_and_never_flattens_an_included_file() {
		// The included file has no final newline; even so, render() must emit only the OWN content —
		// the directive and the surrounding parent — byte-for-byte, never the included body (so there
		// is no `1[core]`-style concatenation, because the included raw is not rendered at all).
		let resolver = MapResolver::default().with("/etc/inc.cfg", "[core]\n\tx = 1");
		let source = "[include]\n\tpath = inc.cfg\n[core]\n\ty = 2\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		// Reads see the included value...
		assert_eq!(config.get_string("core", None, "x"), Some("1"));
		assert_eq!(config.get_string("core", None, "y"), Some("2"));
		// ...but render round-trips the own source exactly, with the include never flattened in.
		let rendered = config.render();
		assert_eq!(rendered, source);
		assert!(
			!rendered.contains("x = 1"),
			"flattened include: {rendered:?}"
		);
	}

	#[test]
	fn expand_includes_future_is_send() {
		// Guard the crate's Send-futures invariant: an expansion driven by a Send+Sync resolver must
		// itself be a Send future (so a multi-threaded runtime can spawn it).
		fn assert_send<T: Send>(_: T) {}
		let resolver = MapResolver::default();
		let mut config = GitConfigSource::new();
		let context = ctx(None, None);
		assert_send(config.expand_includes(Path::new("/etc"), &context, &resolver));
	}

	#[test]
	fn valueless_include_path_is_fatal_when_it_would_be_read() {
		let resolver = MapResolver::default();
		// An unconditional `[include] path` with no value is always processed, so it errors (git:
		// `missing value for 'include.path'`).
		let mut config = GitConfigSource::parse("[include]\n\tpath\n").unwrap();
		let err =
			block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap_err();
		assert!(matches!(err, ConfigError::IncludeMissingValue), "{err:?}");

		// A valueless `includeIf` whose condition does NOT match is never read — so no error (git
		// reads the path only on a match). gitdir=None makes the condition non-matching, and the
		// directive stays queryable (as a bare/valueless key under its condition subsection).
		let source = "[includeIf \"gitdir:/x/\"]\n\tpath\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(
			config.get_raw("includeif", Some("gitdir:/x/"), "path"),
			Some(None)
		);
	}

	#[test]
	fn nested_includes_resolve_relative_to_each_file() {
		// The nested include's relative path resolves against the *nested* file's directory.
		let resolver = MapResolver::default()
			.with(
				"/etc/a.cfg",
				"[core]\n\ta = 1\n[include]\n\tpath = sub/b.cfg\n",
			)
			.with("/etc/sub/b.cfg", "[core]\n\tb = 1\n");
		let source = "[include]\n\tpath = a.cfg\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(config.get_string("core", None, "a"), Some("1"));
		assert_eq!(config.get_string("core", None, "b"), Some("1"));
	}

	#[test]
	fn an_existing_file_beyond_depth_10_is_fatal() {
		// top -> c1 -> ... -> c10 -> c11, all existing: reading the 11th nested include (depth 11)
		// exceeds git's cap. The error fires on the read, so c11's own (missing) tail never matters.
		let mut resolver = MapResolver::default();
		for i in 1..=11 {
			resolver = resolver.with(
				&format!("/etc/c{i}.cfg"),
				&format!("[include]\n\tpath = c{}.cfg\n", i + 1),
			);
		}
		let mut config = GitConfigSource::parse("[include]\n\tpath = c1.cfg\n").unwrap();
		let err =
			block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap_err();
		assert!(matches!(err, ConfigError::IncludeDepthExceeded), "{err:?}");
	}

	#[test]
	fn a_missing_tail_at_would_be_depth_11_is_skipped_not_fatal() {
		// top -> c1 -> ... -> c10, where c10 sets a value and references a MISSING c11. git's access
		// check precedes its depth check, so the absent c11 is silently skipped — no depth error — and
		// c10's content is still applied.
		let mut resolver = MapResolver::default();
		for i in 1..=10 {
			let text = if i == 10 {
				"[core]\n\treached = 10\n[include]\n\tpath = c11.cfg\n".to_owned()
			} else {
				format!("[include]\n\tpath = c{}.cfg\n", i + 1)
			};
			resolver = resolver.with(&format!("/etc/c{i}.cfg"), &text);
		}
		let mut config = GitConfigSource::parse("[include]\n\tpath = c1.cfg\n").unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap();
		assert_eq!(config.get_string("core", None, "reached"), Some("10"));
	}

	#[test]
	fn self_referential_cycle_hits_the_depth_cap() {
		let resolver = MapResolver::default().with("/etc/loop.cfg", "[include]\n\tpath = loop.cfg\n");
		let mut config = GitConfigSource::parse("[include]\n\tpath = loop.cfg\n").unwrap();
		let err =
			block_on(config.expand_includes(Path::new("/etc"), &ctx(None, None), &resolver)).unwrap_err();
		assert!(matches!(err, ConfigError::IncludeDepthExceeded), "{err:?}");
	}

	// --- includeIf gitdir matching (through expand_includes) ---

	#[test]
	fn includeif_gitdir_applies_only_on_match() {
		let gitdir = PathBuf::from("/home/me/work/proj/.git");
		let resolver = MapResolver::default().with("/etc/work.cfg", "[user]\n\temail = work\n");

		// A parent dir with a trailing slash matches a gitdir beneath it.
		let matched = expand(
			"[includeIf \"gitdir:/home/me/work/\"]\n\tpath = work.cfg\n",
			"/etc",
			&ctx(None, Some(gitdir.as_path())),
			&resolver,
		);
		assert_eq!(matched.get_string("user", None, "email"), Some("work"));

		// A non-matching prefix does not apply, and gitdir=None never matches.
		let unmatched = expand(
			"[includeIf \"gitdir:/home/me/play/\"]\n\tpath = work.cfg\n",
			"/etc",
			&ctx(None, Some(gitdir.as_path())),
			&resolver,
		);
		assert!(unmatched.get_string("user", None, "email").is_none());
		let no_gitdir = expand(
			"[includeIf \"gitdir:/home/me/work/\"]\n\tpath = work.cfg\n",
			"/etc",
			&ctx(None, None),
			&resolver,
		);
		assert!(no_gitdir.get_string("user", None, "email").is_none());
	}

	// --- gitdir_matches unit tests ---

	fn gm(pattern: &str, gitdir: &str, icase: bool) -> bool {
		let gitdir = PathBuf::from(gitdir);
		let c = ctx(Some(Path::new("/home/me")), Some(gitdir.as_path()));
		gitdir_matches(pattern, Path::new("/etc/inc"), &c, icase)
	}

	#[test]
	fn gitdir_pattern_rules() {
		// Trailing slash -> append `**`: parent dir matches anything under it (but not itself).
		assert!(gm("/home/me/work/", "/home/me/work/proj/.git", false));
		assert!(!gm("/home/me/work/", "/home/me/work", false));
		// Explicit `**`.
		assert!(gm("/home/me/**/.git", "/home/me/a/b/.git", false));
		// No slash -> `**/<pat>`.
		assert!(gm("proj", "/home/me/work/proj", false));
		assert!(!gm("proj", "/home/me/work/other", false));
		// `~/` expands against home.
		assert!(gm("~/work/", "/home/me/work/proj/.git", false));
		// A bare `~` is home itself: it matches a gitdir equal to home, and nothing beneath it.
		assert!(gm("~", "/home/me", false));
		assert!(!gm("~", "/home/me/work/.git", false));
		// Case-insensitive variant folds ASCII case; the sensitive one does not.
		assert!(gm("/home/ME/work/", "/home/me/work/x/.git", true));
		assert!(!gm("/home/ME/work/", "/home/me/work/x/.git", false));
	}

	#[test]
	fn gitdir_dot_relative_pattern_anchors_to_including_dir() {
		// `./sub/` is relative to the including file's dir (`/etc/inc`), matched literally there.
		assert!(gm("./sub/", "/etc/inc/sub/repo/.git", false));
		assert!(!gm("./sub/", "/etc/other/sub/repo/.git", false));
	}

	#[test]
	fn gitdir_pattern_with_bracket_class() {
		// A `[0-9]` bracket class in a gitdir pattern matches one digit component char.
		assert!(gm("/work/proj[0-9]/", "/work/proj7/.git", false));
		assert!(!gm("/work/proj[0-9]/", "/work/projx/.git", false));
	}

	#[test]
	fn gitdir_pattern_with_posix_class() {
		// A `[[:digit:]]` POSIX class in a gitdir pattern matches one digit component char.
		assert!(gm("/work/repo[[:digit:]]/", "/work/repo7/.git", false));
		assert!(!gm("/work/repo[[:digit:]]/", "/work/repox/.git", false));
	}

	#[test]
	fn gitdir_pattern_with_unterminated_bracket_never_matches() {
		// A malformed `[` makes git's wildmatch abort — the condition never matches, even against a
		// gitdir that contains a literal `[`.
		assert!(!gm("/repos/repo[", "/repos/repo[/.git", false));
	}

	// --- onbranch matching ---

	/// Evaluate `onbranch:<pattern>` against `branch` through the full condition dispatch.
	fn onbranch(pattern: &str, branch: Option<&str>) -> bool {
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch,
			remote_urls: None,
		};
		condition_matches(&format!("onbranch:{pattern}"), Path::new("/etc"), &c)
	}

	#[test]
	fn onbranch_matches_short_branch_with_pathname_wildmatch() {
		let b = Some("feature/foo");
		// Exact, single-`*` (stops at `/`), `**` (crosses `/`) all match.
		assert!(onbranch("feature/foo", b));
		assert!(onbranch("feature/*", b));
		assert!(onbranch("feature/**", b));
		// Trailing `/` appends `**`, so the namespace matches everything beneath it.
		assert!(onbranch("feature/", b));
		// Unlike gitdir, onbranch does NOT prepend `**/`: a bare segment does not match a deeper branch.
		assert!(!onbranch("foo", b));
		// A non-matching pattern, and a single `*` that would have to cross `/`.
		assert!(!onbranch("other", b));
		assert!(!onbranch("*", b));
		// Matching is case-sensitive (branch names are).
		assert!(!onbranch("Feature/foo", b));
	}

	#[test]
	fn onbranch_never_matches_without_a_branch() {
		// A detached HEAD or bare repo supplies no branch, so every onbranch condition is false.
		assert!(!onbranch("main", None));
		assert!(!onbranch("*", None));
	}

	#[test]
	fn onbranch_applies_include_through_expand() {
		let resolver = MapResolver::default().with("/etc/branch.cfg", "[user]\n\temail = onbr\n");
		let source = "[includeIf \"onbranch:feature/*\"]\n\tpath = branch.cfg\n";
		let on = IncludeContext {
			home: None,
			gitdir: None,
			branch: Some("feature/x"),
			remote_urls: None,
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &on, &resolver)).unwrap();
		assert_eq!(config.get_string("user", None, "email"), Some("onbr"));

		// A non-matching branch does not apply it.
		let off = IncludeContext {
			branch: Some("main"),
			..on
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &off, &resolver)).unwrap();
		assert!(config.get_string("user", None, "email").is_none());
	}

	// --- hasconfig:remote.*.url matching ---

	/// Evaluate `hasconfig:remote.*.url:<glob>` against `urls` through the full condition dispatch.
	fn hasconfig(glob: &str, urls: Option<&[&str]>) -> bool {
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: urls,
		};
		condition_matches(
			&format!("hasconfig:remote.*.url:{glob}"),
			Path::new("/etc"),
			&c,
		)
	}

	#[test]
	fn hasconfig_matches_any_url_with_anchored_pathname_wildmatch() {
		let urls: &[&str] = &[
			"git@example:other.git",
			"https://github.com/example/repo.git",
		];
		// `**` crosses `/`, so the value-glob matches the whole URL.
		assert!(hasconfig("https://github.com/**", Some(urls)));
		// A single `*` stops at `/`, so it cannot span the URL's path — no match (git's WM_PATHNAME).
		assert!(!hasconfig("https://github.com/*", Some(urls)));
		// The glob is anchored (whole-string): a bare host without the trailing `**` does not match.
		assert!(!hasconfig("https://github.com/", Some(urls)));
		// Case-sensitive value matching.
		assert!(!hasconfig("https://GITHUB.com/**", Some(urls)));
		// No URL matches this host at all.
		assert!(!hasconfig("https://gitlab.com/**", Some(urls)));
	}

	#[test]
	fn hasconfig_never_matches_without_urls() {
		assert!(!hasconfig("https://github.com/**", None));
		assert!(!hasconfig("https://github.com/**", Some(&[])));
	}

	#[test]
	fn hasconfig_only_the_literal_remote_url_form_is_recognised() {
		// git special-cases exactly `hasconfig:remote.*.url:`; a wildcarded or differently-cased var
		// glob, or a general `hasconfig:<var>:<value>`, is an unrecognised conditional → false.
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let m = |cond: &str| condition_matches(cond, Path::new("/etc"), &c);
		assert!(m("hasconfig:remote.*.url:https://github.com/**"));
		assert!(!m("hasconfig:remote.?.url:https://github.com/**"));
		assert!(!m("hasconfig:remote.*.URL:https://github.com/**"));
		assert!(!m("hasconfig:some.key:https://github.com/**"));
		assert!(!m("hasconfig:remote.*.pushurl:https://github.com/**"));
	}

	#[test]
	fn hasconfig_applies_include_through_expand() {
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let resolver = MapResolver::default().with("/etc/id.cfg", "[user]\n\temail = ghid\n");
		let source = "[includeIf \"hasconfig:remote.*.url:https://github.com/**\"]\n\tpath = id.cfg\n";
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &c, &resolver)).unwrap();
		assert_eq!(config.get_string("user", None, "email"), Some("ghid"));
	}

	#[test]
	fn hasconfig_included_file_setting_a_remote_url_is_fatal() {
		// git forbids a hasconfig-included file from setting a `remote.<name>.url` (it would circularly
		// feed the condition). The engine enforces this on the matched path.
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let resolver = MapResolver::default().with(
			"/etc/id.cfg",
			"[remote \"sneaky\"]\n\turl = https://elsewhere/x.git\n[user]\n\temail = ghid\n",
		);
		let source = "[includeIf \"hasconfig:remote.*.url:https://github.com/**\"]\n\tpath = id.cfg\n";
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		let err = block_on(config.expand_includes(Path::new("/etc"), &c, &resolver)).unwrap_err();
		assert!(
			matches!(err, ConfigError::HasconfigIncludeSetsRemoteUrl),
			"{err:?}"
		);
	}

	#[test]
	fn hasconfig_included_file_forbidden_url_is_indirect_too() {
		// A file the hasconfig include pulls in via a plain `[include]` may not set a remote url either
		// (git: "directly or indirectly"). The recursive expansion folds the nested url in, so the guard
		// catches it.
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let resolver = MapResolver::default()
			.with(
				"/etc/wrapper.cfg",
				"[include]\n\tpath = deep.cfg\n[user]\n\temail = ghid\n",
			)
			.with(
				"/etc/deep.cfg",
				"[remote \"deep\"]\n\turl = https://deep/z.git\n",
			);
		let source =
			"[includeIf \"hasconfig:remote.*.url:https://github.com/**\"]\n\tpath = wrapper.cfg\n";
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		let err = block_on(config.expand_includes(Path::new("/etc"), &c, &resolver)).unwrap_err();
		assert!(
			matches!(err, ConfigError::HasconfigIncludeSetsRemoteUrl),
			"{err:?}"
		);
	}

	#[test]
	fn hasconfig_included_bare_remote_url_is_allowed() {
		// A bare `remote.url` (no subsection) is not a `remote.<name>.url`, so git neither collects nor
		// forbids it — the include applies without error.
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let resolver = MapResolver::default().with(
			"/etc/id.cfg",
			"[remote]\n\turl = https://bare/x.git\n[user]\n\temail = ghid\n",
		);
		let source = "[includeIf \"hasconfig:remote.*.url:https://github.com/**\"]\n\tpath = id.cfg\n";
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		block_on(config.expand_includes(Path::new("/etc"), &c, &resolver)).unwrap();
		assert_eq!(config.get_string("user", None, "email"), Some("ghid"));
	}

	#[test]
	fn plain_include_setting_a_remote_url_is_not_forbidden() {
		// The paradox guard is specific to hasconfig includes; an ordinary `[include]` may carry remote
		// urls freely (indeed that is how git's hasconfig sees urls introduced by earlier includes).
		let resolver = MapResolver::default().with(
			"/etc/urls.cfg",
			"[remote \"origin\"]\n\turl = https://github.com/x.git\n",
		);
		let source = "[include]\n\tpath = urls.cfg\n";
		let config = expand(source, "/etc", &ctx(None, None), &resolver);
		assert_eq!(
			config.get_string("remote", Some("origin"), "url"),
			Some("https://github.com/x.git")
		);
	}

	#[test]
	fn hasconfig_url_guard_fires_before_a_later_bad_include() {
		// A hasconfig target sets a `remote.<name>.url` and *then* has a bare (valueless) include. git
		// reads top-to-bottom, so the URL paradox fatals before the bad include is reached — probed:
		// git errors with the remote-URL message, not `missing value`. The positional guard matches
		// this; a post-hoc scan would surface `IncludeMissingValue` from expanding the later include.
		let urls: &[&str] = &["https://github.com/example/repo.git"];
		let resolver = MapResolver::default().with(
			"/etc/id.cfg",
			"[remote \"sneaky\"]\n\turl = https://elsewhere/x.git\n[include]\n\tpath\n",
		);
		let source = "[includeIf \"hasconfig:remote.*.url:https://github.com/**\"]\n\tpath = id.cfg\n";
		let c = IncludeContext {
			home: None,
			gitdir: None,
			branch: None,
			remote_urls: Some(urls),
		};
		let mut config = GitConfigSource::parse(source).unwrap();
		let err = block_on(config.expand_includes(Path::new("/etc"), &c, &resolver)).unwrap_err();
		assert!(
			matches!(err, ConfigError::HasconfigIncludeSetsRemoteUrl),
			"{err:?}"
		);
	}

	#[test]
	fn malformed_condition_patterns_never_match() {
		// git aborts the whole wildmatch (`WM_ABORT_ALL`) on a *terminated-but-unknown* POSIX class or a
		// trailing backslash, so such an `onbranch:`/`hasconfig:` pattern matches nothing — even text a
		// literal reading of the construct would match (branch `b]`, url `a\`).
		assert!(!onbranch("[[:bogus:]]", Some("b]")));
		assert!(!hasconfig("[[:bogus:]]", Some(&["b]"])));
		assert!(!hasconfig("a\\", Some(&["a\\"])));
		// A well-formed class still matches, so the abort is scoped to the malformed case.
		assert!(onbranch("[[:digit:]]", Some("7")));
	}

	#[test]
	fn bracket_edge_cases_match_git() {
		// A `[:` with no `:]` terminator is NOT malformed: git treats `[` as an ordinary set member, so
		// `[[:abc]` is the set `{[ : a b c}` and matches branch `a` (probed against git 2.50.1).
		assert!(onbranch("[[:abc]", Some("a")));
		assert!(!onbranch("[[:abc]", Some("z")));
		// A descending range matches its low endpoint (git reads it as a literal before the `-`): `[b-a]`
		// matches `b`, but not `a` (the empty span) — probed.
		assert!(onbranch("[b-a]", Some("b")));
		assert!(!onbranch("[b-a]", Some("a")));
		// An ascending range is unchanged.
		assert!(onbranch("[a-c]", Some("b")));
	}

	// --- wildmatch unit tests ---

	#[test]
	fn wildmatch_pathname_semantics() {
		let m = |p: &str, t: &str| wildmatch(p.as_bytes(), t.as_bytes(), false);
		// `*` matches within a segment but not across `/`.
		assert!(m("a*c", "abc"));
		assert!(m("a*c", "ac"));
		assert!(!m("a*c", "a/c"));
		assert!(!m("*", "a/b"));
		assert!(m("*", "abc"));
		// `**` crosses `/` (any number of segments, including none).
		assert!(m("a/**/b", "a/b"));
		assert!(m("a/**/b", "a/x/y/b"));
		assert!(m("**", "a/b/c"));
		assert!(m("**/b", "b"));
		assert!(m("**/b", "x/y/b"));
		// `**` matches complete components only — not a partial one.
		assert!(!m("**/b", "xb"));
		// A `**` followed by a single `*` still backtracks correctly (would fail single-star memory).
		assert!(m("**/*x", "a/bx"));
		assert!(m("a/**/*.txt", "a/b/c/d.txt"));
		// `?` matches one non-`/` byte.
		assert!(m("a?c", "abc"));
		assert!(!m("a?c", "a/c"));
		assert!(!m("a?c", "ac"));
		// Literals must match exactly, and the whole text is consumed (anchored).
		assert!(m("abc", "abc"));
		assert!(!m("abc", "abcd"));
		assert!(!m("abc", "ab"));
		// Case folding.
		assert!(wildmatch(b"ABC", b"abc", true));
		assert!(!wildmatch(b"ABC", b"abc", false));
	}

	#[test]
	fn wildmatch_bracket_classes() {
		let m = |p: &str, t: &str| wildmatch(p.as_bytes(), t.as_bytes(), false);
		// Range.
		assert!(m("proj[0-9]", "proj7"));
		assert!(!m("proj[0-9]", "projx"));
		assert!(m("v[a-z]", "vq"));
		// Set.
		assert!(m("[abc]", "b"));
		assert!(!m("[abc]", "d"));
		// Negation, both spellings.
		assert!(m("[!0-9]", "x"));
		assert!(!m("[!0-9]", "5"));
		assert!(m("[^abc]", "z"));
		assert!(!m("[^abc]", "a"));
		// A class never matches `/`.
		assert!(!m("[a/]", "/"));
		// A `]` immediately after `[` is a literal member.
		assert!(m("[]x]", "]"));
		assert!(m("[]x]", "x"));
		// Case-insensitive class folding.
		assert!(wildmatch(b"[a-z]", b"Q", true));
		// A malformed (unterminated) `[` makes the whole pattern non-matching (git aborts the match),
		// so it does NOT match text that happens to contain a literal `[`.
		assert!(!m("a[b", "a[b"));
		assert!(!m("repo[", "repo["));
	}

	#[test]
	fn wildmatch_posix_classes() {
		let m = |p: &str, t: &str| wildmatch(p.as_bytes(), t.as_bytes(), false);
		// `[[:digit:]]` matches one digit (its inner `]` does not close the outer class).
		assert!(m("repo[[:digit:]]", "repo7"));
		assert!(!m("repo[[:digit:]]", "repox"));
		assert!(m("[[:alpha:]]", "q"));
		assert!(!m("[[:alpha:]]", "5"));
		assert!(m("[[:alnum:]]", "5"));
		assert!(m("[[:alnum:]]", "z"));
		assert!(m("x[[:space:]]y", "x y"));
		assert!(m("[[:upper:]]", "Q"));
		assert!(!m("[[:upper:]]", "q"));
		// A POSIX class combines with ordinary members in the same bracket.
		assert!(m("[[:digit:]abc]", "b"));
		assert!(m("[[:digit:]abc]", "3"));
		assert!(!m("[[:digit:]abc]", "z"));
		// Negated POSIX class.
		assert!(m("[![:digit:]]", "a"));
		assert!(!m("[![:digit:]]", "3"));
		// `[:upper:]` under icase folds so either case matches.
		assert!(wildmatch(b"[[:upper:]]", b"q", true));
	}

	#[test]
	fn wildmatch_aborts_on_malformed_constructs() {
		// git's wildmatch returns `WM_ABORT_ALL` for a trailing backslash or a terminated-but-unknown
		// POSIX class, making the whole pattern match nothing — even text a literal reading would match.
		// (This also corrects the shared matcher for `gitdir:`, which routes through the same code.)
		// Trailing backslash: `a\` must not match `a\` (which a literal-backslash reading would).
		assert!(!wildmatch(b"a\\", b"a\\", false));
		assert!(!wildmatch(b"*\\", b"x\\", false));
		// Terminated-but-unknown POSIX class `[[:bogus:]]` aborts: must not match `b]`.
		assert!(!wildmatch(b"[[:bogus:]]", b"b]", false));
		// An escaped interior byte is still a literal (only a *trailing* backslash aborts).
		assert!(wildmatch(b"a\\bc", b"abc", false));
	}

	#[test]
	fn wildmatch_bracket_edge_cases_match_git() {
		let m = |p: &[u8], t: &[u8]| wildmatch(p, t, false);
		// `[:` with no `:]` terminator is an ordinary set, `[` included as a literal member. So
		// `[[:abc]` = `{[ : a b c}` — matches each of those one-byte texts, nothing else.
		assert!(m(b"[[:abc]", b"["));
		assert!(m(b"[[:abc]", b":"));
		assert!(m(b"[[:abc]", b"a"));
		assert!(m(b"[[:abc]", b"c"));
		assert!(!m(b"[[:abc]", b"z"));
		// A descending range matches only its low endpoint (tested as a literal before the `-`).
		assert!(m(b"[b-a]", b"b"));
		assert!(!m(b"[b-a]", b"a"));
		assert!(!m(b"[b-a]", b"c"));
		// An ascending range spans as usual, and its endpoints match.
		assert!(m(b"[a-c]", b"a"));
		assert!(m(b"[a-c]", b"b"));
		assert!(m(b"[a-c]", b"c"));
		assert!(!m(b"[a-c]", b"d"));
	}

	#[test]
	fn wildmatch_is_not_exponential_on_pathological_patterns() {
		// `*a` × 28 followed by `b`, against a long text with no `b`: a recursive backtracker is
		// exponential here (a config-load DoS); the DP stays fast. Just assert it returns (quickly).
		let pattern = "*a".repeat(28) + "b";
		let text = "a".repeat(4096);
		assert!(!wildmatch(pattern.as_bytes(), text.as_bytes(), false));
	}

	#[test]
	fn wildmatch_large_literal_inputs_stay_linear() {
		// A `hasconfig` glob and URL are attacker-controlled config values. A full O(tokens × text) DP
		// took seconds on a ~20 KiB literal pair (a config-load hang); anchoring the star-free / literal
		// runs keeps these linear, so this returns instantly rather than doing ~n² work.
		let lit = "a".repeat(20_000);
		assert!(wildmatch(lit.as_bytes(), lit.as_bytes(), false));
		let longer = format!("{lit}b");
		assert!(!wildmatch(lit.as_bytes(), longer.as_bytes(), false));
		// One star between long literal runs is linear too (both runs anchor to the text's ends).
		let glob = format!("{lit}*{lit}");
		let url = format!("{lit}MIDDLE{lit}");
		assert!(wildmatch(glob.as_bytes(), url.as_bytes(), false));
		assert!(!wildmatch(glob.as_bytes(), lit.as_bytes(), false));
	}

	/// Drive `future` to completion on a fresh current-thread runtime — the resolver's `read` is the
	/// only await point and does no real I/O, so no timer/IO drivers are needed.
	fn block_on<F: std::future::Future>(future: F) -> F::Output {
		tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap()
			.block_on(future)
	}
}
