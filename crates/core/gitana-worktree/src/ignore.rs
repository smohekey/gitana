//! A from-scratch `.gitignore` matcher.
//!
//! Supports per-directory ignore files, `#` comments, `!` negation, trailing-`/`
//! directory-only patterns, leading-`/` and internal-`/` anchoring, basename
//! matching for unanchored patterns, the `*` `?` `[...]` `**` globs, and backslash
//! escapes (a leading `\#`/`\!`, and `\*`/`\?`/`\[`/escaped-trailing-space to match
//! a metacharacter literally). Last matching pattern (deepest file, then last line)
//! wins. Not handled: `.git/info/exclude` and the global excludes file.

/// One ignore pattern.
struct Pattern {
	negated: bool,
	dir_only: bool,
	anchored: bool,
	glob: String,
}

/// The patterns from one directory's `.gitignore`, tagged with that directory's
/// path relative to the working-tree root (`""` for the root).
pub(crate) struct DirIgnore {
	dir: String,
	patterns: Vec<Pattern>,
}

/// Parse a `.gitignore` whose directory is `dir` (relative to the root).
pub(crate) fn parse(text: &str, dir: &str) -> DirIgnore {
	let mut patterns = Vec::new();
	for raw in text.lines() {
		let line = trim_trailing_spaces(raw);
		if line.is_empty() {
			continue;
		}
		// A leading backslash escapes a `#` or `!` so it is a literal first character rather than a
		// comment or negation marker: `\#hash` matches the file `#hash`, `\!keep` the file `!keep`.
		let (negated, body) = match line.strip_prefix('\\') {
			Some(rest) if rest.starts_with('#') || rest.starts_with('!') => (false, rest),
			_ if line.starts_with('#') => continue,
			_ => match line.strip_prefix('!') {
				Some(rest) => (true, rest),
				None => (false, line),
			},
		};
		let dir_only = body.ends_with('/');
		let mut glob = body.strip_suffix('/').unwrap_or(body);
		let anchored = match glob.strip_prefix('/') {
			Some(rest) => {
				glob = rest;
				true
			}
			None => glob.contains('/'),
		};
		patterns.push(Pattern {
			negated,
			dir_only,
			anchored,
			glob: glob.to_owned(),
		});
	}
	DirIgnore {
		dir: dir.to_owned(),
		patterns,
	}
}

/// Trim trailing spaces that are *not* backslash-escaped, matching git: a trailing space is dropped
/// unless preceded by an odd number of backslashes (`file\ ` keeps its escaped space). Leading
/// whitespace is significant and left untouched; only lines end with `\n`/`\r\n` (already split off).
fn trim_trailing_spaces(line: &str) -> &str {
	let bytes = line.as_bytes();
	let mut end = bytes.len();
	while end > 0 && bytes[end - 1] == b' ' {
		let backslashes = bytes[..end - 1]
			.iter()
			.rev()
			.take_while(|&&b| b == b'\\')
			.count();
		if backslashes % 2 == 1 {
			break; // the space is escaped — keep it and everything before it
		}
		end -= 1;
	}
	&line[..end]
}

/// Whether `path` (relative to root) is ignored by the accumulated ignore files in
/// `stack` (root first). Last matching pattern wins; a negated match re-includes.
pub(crate) fn is_ignored(path: &str, is_dir: bool, stack: &[DirIgnore]) -> bool {
	let mut ignored = false;
	for level in stack {
		let Some(rel) = strip_dir(path, &level.dir) else {
			continue;
		};
		for pattern in &level.patterns {
			if pattern.dir_only && !is_dir {
				continue;
			}
			let subject = if pattern.anchored {
				rel
			} else {
				rel.rsplit('/').next().unwrap_or(rel)
			};
			if glob_match(pattern.glob.as_bytes(), subject.as_bytes(), false, true) {
				ignored = !pattern.negated;
			}
		}
	}
	ignored
}

/// Whether the **file** `path` (root-relative) is *included* by non-cone sparse-checkout patterns
/// `patterns` (a single root-level list, parsed via [`parse`] with `dir == ""`). git's sparse-checkout
/// non-cone mode reuses the gitignore pattern machinery — same anchoring / basename / dir-only / glob
/// rules — but the semantics differ from [`is_ignored`] and it evaluates **hierarchically**, level by
/// level, exactly like git's `clear_ce_flags` descent (verified against git 2.50.1):
///
/// - each ancestor directory of `path`, and finally `path` itself as a file, is a *level*;
/// - at each level the *last* pattern that matches that level (a directory, or the file at the last
///   level) sets the verdict — a matching pattern means *included*, a `!` negation means *excluded*;
/// - a level with **no** matching pattern **inherits** its parent's verdict (a directory match includes
///   its whole subtree — `/foo/` includes `foo/sub/h.txt`); the root default is *not included*.
///
/// The level-by-level inheritance is what makes `!baz/` fail to exclude the file `baz/d.log` that an
/// earlier `*.log` included: at the `baz` directory level `!baz/` excludes, but at the file level
/// `*.log` matches (and the dir-only `!baz/` does not), so the file's own level re-includes it. And it
/// is what makes a deeper `!/foo/sub/` exclude `foo/sub/*` while leaving `foo/*` included.
pub(crate) fn sparse_match(path: &str, patterns: &DirIgnore, fold: bool) -> bool {
	let mut included = false;
	let mut idx = 0;
	loop {
		let (level, is_dir, last) = match path[idx..].find('/') {
			Some(next) => (&path[..idx + next], true, false),
			None => (path, false, true),
		};
		if let Some(verdict) = last_matching(patterns, level, is_dir, fold) {
			included = verdict;
		}
		if last {
			return included;
		}
		idx = level.len() + 1;
	}
}

/// The verdict (`true` = included, `false` = excluded) of the *last* pattern that matches `subject` at
/// this level, or `None` when no pattern matches (so the level inherits its parent's verdict). `fold`
/// matches case-insensitively (git's `core.ignoreCase`).
fn last_matching(patterns: &DirIgnore, subject: &str, is_dir: bool, fold: bool) -> Option<bool> {
	let mut verdict = None;
	for pattern in &patterns.patterns {
		if matches_subject(pattern, subject, is_dir, fold) {
			verdict = Some(!pattern.negated);
		}
	}
	verdict
}

/// Whether `pattern` matches `subject` (a path component or full path) treated as a file or directory —
/// the same rule [`is_ignored`] applies per level: a dir-only pattern needs `is_dir`, an anchored
/// pattern matches the full relative subject, an unanchored one matches the basename. `fold` matches
/// case-insensitively (git's `core.ignoreCase`).
fn matches_subject(pattern: &Pattern, subject: &str, is_dir: bool, fold: bool) -> bool {
	if pattern.dir_only && !is_dir {
		return false;
	}
	let glob_subject = if pattern.anchored {
		subject
	} else {
		subject.rsplit('/').next().unwrap_or(subject)
	};
	glob_match(pattern.glob.as_bytes(), glob_subject.as_bytes(), fold, true)
}

fn strip_dir<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
	if dir.is_empty() {
		Some(path)
	} else {
		path
			.strip_prefix(dir)
			.and_then(|rest| rest.strip_prefix('/'))
	}
}

/// Whether two bytes are equal, folding ASCII case when `fold` (git's `core.ignoreCase`).
fn byte_eq(a: u8, b: u8, fold: bool) -> bool {
	if fold {
		a.eq_ignore_ascii_case(&b)
	} else {
		a == b
	}
}

/// Match `text` against the glob `pat`. `fold` folds ASCII case (`core.ignoreCase`). `pathname`
/// selects the two git modes: `true` is FNM_PATHNAME (gitignore, sparse-checkout, and `:(glob)`
/// pathspecs) — `*`/`?`/`[]` do NOT cross `/`, and `**` is the special path-spanning glob; `false` is a
/// plain default pathspec — `*`/`?`/`[]` DO cross `/`, and `**` is not special (just consecutive `*`).
pub(crate) fn glob_match(pat: &[u8], text: &[u8], fold: bool, pathname: bool) -> bool {
	// No wildcard has been seen in the (empty) leading component, so a leading `**` is a genuine globstar.
	let mut memo = Memo::default();
	glob_match_inner(pat, text, fold, pathname, false, &mut memo)
}

/// Memo of `(remaining pattern length, remaining text length, wild_in_component)` states already known to
/// *not* match. Every recursive call matches a **suffix** of the original pattern against a suffix of the
/// original text, so those two lengths (with `fold`/`pathname` fixed for the whole match) identify the
/// state uniquely — caching failures turns the otherwise-exponential star backtracking into O(len²) and
/// defeats adversarial patterns like `(*a){28}b` against a long all-`a` name (git stays linear too).
type Memo = std::collections::HashSet<(usize, usize, bool)>;

/// The recursive core of [`glob_match`], wrapping [`glob_match_body`] with the failure memo.
fn glob_match_inner(
	pat: &[u8],
	text: &[u8],
	fold: bool,
	pathname: bool,
	wild_in_component: bool,
	memo: &mut Memo,
) -> bool {
	// The empty-pattern base case is O(1) and need not be cached.
	if pat.is_empty() {
		return text.is_empty();
	}
	let key = (pat.len(), text.len(), wild_in_component);
	if memo.contains(&key) {
		return false;
	}
	let matched = glob_match_body(pat, text, fold, pathname, wild_in_component, memo);
	if !matched {
		memo.insert(key);
	}
	matched
}

/// The match logic. `wild_in_component` records whether any wildcard (`*`, `?`, or a `[…]` class) has
/// already appeared **in the current path component** — reset to `false` at every `/`. This only matters
/// for a `**` run in pathname mode: git treats `**` as a path-spanning globstar solely when it is a whole
/// component preceded only by literals, so `a**` / `ba**` cross `/` but `?**`, `[x]**`, and `*a**` (a
/// wildcard earlier in the same component) collapse to an ordinary within-component `*` (probed vs git
/// 2.50.1).
fn glob_match_body(
	pat: &[u8],
	text: &[u8],
	fold: bool,
	pathname: bool,
	wild_in_component: bool,
	memo: &mut Memo,
) -> bool {
	match pat.first() {
		None => text.is_empty(),
		Some(b'*') => {
			// Consume the whole run of consecutive `*`, then decide once how it behaves.
			let run = pat.iter().take_while(|&&b| b == b'*').count();
			let rest = &pat[run..];
			// A `**` run is the special path-spanning globstar only when: pathname mode is on, the run is
			// two or more stars, no wildcard preceded it in this component (`wild_in_component`), and the run
			// is a whole path component (at the end, or immediately followed by `/`). Any other star run — a
			// single `*`, a `**` not at a component boundary (`a**b`), or one after another wildcard in the
			// component (`?**`, `*a**`) — behaves as a plain `*` that stops at `/` in pathname mode.
			let globstar =
				pathname && run >= 2 && !wild_in_component && matches!(rest.first(), None | Some(&b'/'));
			if globstar {
				// A `**/` consumes the slash too, so it can also match *zero* directories. The recursion is a
				// fresh component, so `wild_in_component` resets to false.
				let after = match rest.first() {
					Some(&b'/') => &rest[1..],
					_ => rest,
				};
				if glob_match_inner(after, text, fold, pathname, false, memo) {
					return true;
				}
				for (i, &c) in text.iter().enumerate() {
					if c == b'/' && glob_match_inner(after, &text[i + 1..], fold, pathname, false, memo) {
						return true;
					}
				}
				// A *trailing* `**` (nothing after it) matches the rest of the text entirely — any remaining
				// bytes. A `**/` (a slash followed the stars) only spans complete directory *components*, so
				// once every `/`-boundary attempt above has failed it must NOT match a partial leftover: e.g.
				// `a**/` matches the file `a` (zero directories, handled above) but not `aa` (probed vs git).
				return rest.is_empty();
			}
			// Plain `*` (or a non-globstar star run collapsing to one): match progressively more of the
			// text. In pathname mode it stops at `/`; without it, `*` consumes everything including `/`. The
			// star is itself a wildcard in this component, so the recursion carries `wild_in_component = true`.
			let mut i = 0;
			loop {
				if glob_match_inner(rest, &text[i..], fold, pathname, true, memo) {
					return true;
				}
				if i >= text.len() || (pathname && text[i] == b'/') {
					return false;
				}
				i += 1;
			}
		}
		Some(b'?') => match text.first() {
			// A `?` is a wildcard in this component — mark it so a following `**` is not a globstar.
			Some(&c) if !pathname || c != b'/' => {
				glob_match_inner(&pat[1..], &text[1..], fold, pathname, true, memo)
			}
			_ => false,
		},
		Some(b'[') => match_class(pat, text, fold, pathname, memo),
		// A backslash escapes the next byte, disabling its glob meaning: `\*`, `\?`, `\[`, and `\ ` match
		// those characters literally (git's gitignore escapes). A trailing *lone* backslash makes git's
		// wildmatch fail outright (probed vs git 2.50.1: a `.gitignore` line `foo\` ignores nothing, and a
		// pathspec `?\` selects only the literally-spelled `?\`) — so the whole match fails here, and the
		// caller's separate literal pass handles the exact spelling.
		Some(b'\\') => match pat.get(1) {
			Some(&escaped) => match text.first() {
				// An escaped byte is a literal; it carries `wild_in_component` forward, resetting at a `/`.
				Some(&t) if byte_eq(t, escaped, fold) => glob_match_inner(
					&pat[2..],
					&text[1..],
					fold,
					pathname,
					wild_in_component && escaped != b'/',
					memo,
				),
				_ => false,
			},
			None => false,
		},
		Some(&c) => match text.first() {
			// A literal byte carries `wild_in_component` forward, but a `/` ends the component and resets it.
			Some(&t) if byte_eq(t, c, fold) => glob_match_inner(
				&pat[1..],
				&text[1..],
				fold,
				pathname,
				wild_in_component && c != b'/',
				memo,
			),
			_ => false,
		},
	}
}

fn match_class(pat: &[u8], text: &[u8], fold: bool, pathname: bool, memo: &mut Memo) -> bool {
	let c = match text.first() {
		// A class matches one character; in pathname mode it never matches `/`.
		Some(&c) if !pathname || c != b'/' => c,
		_ => return false,
	};
	let mut i = 1;
	let negate = matches!(pat.get(1), Some(b'!') | Some(b'^'));
	if negate {
		i = 2;
	}
	let mut matched = false;
	let mut members = 0usize;
	// A `]` immediately after `[` (or `[!`/`[^`) is a literal class member, not the closing delimiter, so
	// `[]a]` matches `]` or `a` and `[!]]` matches any byte but `]` (git). It may also be the *low end of a
	// range* — `[]-a]` matches `]` through `a` (probed vs git 2.50.1) — so consume it as a range when a
	// `]-x` follows, and otherwise as a lone member, before the terminator-sensitive loop below.
	if pat.get(i) == Some(&b']') {
		if pat.get(i + 1) == Some(&b'-') && pat.get(i + 2).is_some_and(|&b| b != b']') {
			let (hi, hi_end) = class_atom(pat, i + 2);
			if in_range(b']', hi, c, fold) {
				matched = true;
			}
			members += 1;
			i = hi_end;
		} else {
			if byte_eq(b']', c, fold) {
				matched = true;
			}
			members += 1;
			i += 1;
		}
	}
	while i < pat.len() && pat[i] != b']' {
		members += 1;
		// A POSIX character class `[:name:]` (e.g. `[[:digit:]]`) — recognise it before the `]` in its
		// closing `:]` is mistaken for the end of the outer class.
		if pat[i] == b'['
			&& pat.get(i + 1) == Some(&b':')
			&& let Some(rel) = pat[i + 2..].windows(2).position(|w| w == b":]")
		{
			match posix_class_match(&pat[i + 2..i + 2 + rel], c, fold) {
				// An UNKNOWN class name (`[:bogus:]`) is malformed: git aborts the whole match, so the
				// pattern selects nothing — even a negated `[![:bogus:]]` matches nothing (probed vs git
				// 2.50.1). Returning false here, before the negation flip below, gives that.
				None => return false,
				Some(true) => matched = true,
				Some(false) => {}
			}
			i += 2 + rel + 2; // skip `[:` + name + `:]`
			continue;
		}
		// Read the low atom — a byte, or a backslash escape (`\-` is a literal `-`, `\]` a member, not the
		// class terminator). It may be the low end of a range `lo-hi` when a `-` and a non-`]` atom follow;
		// both endpoints may themselves be escaped (`[\a-c]` is the range `a`..`c`). Probed vs git 2.50.1.
		let (lo, lo_end) = class_atom(pat, i);
		if pat.get(lo_end) == Some(&b'-') && pat.get(lo_end + 1).is_some_and(|&b| b != b']') {
			let (hi, hi_end) = class_atom(pat, lo_end + 1);
			if in_range(lo, hi, c, fold) {
				matched = true;
			}
			i = hi_end;
		} else {
			if byte_eq(lo, c, fold) {
				matched = true;
			}
			i = lo_end;
		}
	}
	if i >= pat.len() {
		return false; // unterminated class
	}
	if members == 0 {
		return false; // an empty class (`[]`, `[!]`, `[^]`) matches nothing (git reports it unmatched)
	}
	if matched != negate {
		// The class is a wildcard in this component, so a following `**` is not a globstar (`[x]**` stays
		// within its component, unlike `a**`) — carry `wild_in_component = true`.
		glob_match_inner(&pat[i + 1..], &text[1..], fold, pathname, true, memo)
	} else {
		false
	}
}

/// Read one character-class atom at `i`: a backslash escapes the next byte (`\-` is a literal `-`, `\]` a
/// member rather than the class terminator), otherwise the byte stands for itself. Returns the byte and
/// the index just past it. A trailing lone `\` (no next byte) is taken as a literal backslash.
fn class_atom(pat: &[u8], i: usize) -> (u8, usize) {
	if pat[i] == b'\\' && i + 1 < pat.len() {
		(pat[i + 1], i + 2)
	} else {
		(pat[i], i + 1)
	}
}

/// Whether byte `c` is a member of the POSIX character class `[:name:]` (git's wildmatch supports these
/// in gitignore-style patterns). `None` for an **unknown** class name — the caller treats it as a
/// malformed pattern that matches nothing (git aborts the match). Under `fold`, an alpha `c` also tries
/// its opposite case, so `[:upper:]` matches a lowercase letter and vice versa.
fn posix_class_match(name: &[u8], c: u8, fold: bool) -> Option<bool> {
	let member = |c: u8| match name {
		b"alpha" => Some(c.is_ascii_alphabetic()),
		b"digit" => Some(c.is_ascii_digit()),
		b"alnum" => Some(c.is_ascii_alphanumeric()),
		b"upper" => Some(c.is_ascii_uppercase()),
		b"lower" => Some(c.is_ascii_lowercase()),
		b"space" => Some(matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')),
		b"blank" => Some(matches!(c, b' ' | b'\t')),
		b"punct" => Some(c.is_ascii_punctuation()),
		b"cntrl" => Some(c.is_ascii_control()),
		b"xdigit" => Some(c.is_ascii_hexdigit()),
		b"graph" => Some(c.is_ascii_graphic()),
		b"print" => Some(c.is_ascii_graphic() || c == b' '),
		_ => None,
	};
	let direct = member(c)?;
	let folded = fold && c.is_ascii_alphabetic() && member(c ^ 0x20) == Some(true);
	Some(direct || folded)
}

/// Whether byte `c` falls in the class range `lo..=hi`, folding ASCII case when `fold`: an alpha `c`
/// that misses the range is retried in its opposite case, so `[A-Z]` matches `a` and `[a-z]` matches
/// `A` (git's `core.ignoreCase` wildmatch case-fold).
fn in_range(lo: u8, hi: u8, c: u8, fold: bool) -> bool {
	// A DESCENDING range (`[c-a]`, lo > hi) is not empty in git: it matches just its low (first) endpoint
	// literally — `a[c-a]` selects `ac` but not `aa` (probed vs git 2.50.1).
	if lo > hi {
		return byte_eq(lo, c, fold);
	}
	if lo <= c && c <= hi {
		return true;
	}
	if fold && c.is_ascii_alphabetic() {
		let swapped = c ^ 0x20;
		return lo <= swapped && swapped <= hi;
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn globs() {
		assert!(glob_match(b"*.log", b"a.log", false, true));
		assert!(!glob_match(b"*.log", b"a.txt", false, true));
		assert!(!glob_match(b"*.log", b"a/b.log", false, true)); // * does not cross '/'
		assert!(glob_match(b"**/*.log", b"a/b/c.log", false, true));
		assert!(glob_match(b"build/**", b"build/x/y", false, true));
		assert!(glob_match(b"a/**/b", b"a/x/y/b", false, true));
		assert!(glob_match(b"a/**/b", b"a/b", false, true));
		// `**` spans directories only as a whole path component. `a/**b` is *not* such a component, so git
		// treats it as regular asterisks that do not cross '/': it matches `a/xb` but not `a/a/b` (verified
		// against stock git, which reports `a/a/b` untracked). A mis-recursive match here would let removal
		// delete an untracked file it believed ignored.
		assert!(glob_match(b"a/**b", b"a/xb", false, true));
		assert!(!glob_match(b"a/**b", b"a/a/b", false, true));
		assert!(glob_match(b"file?", b"file1", false, true));
		assert!(glob_match(b"[abc].txt", b"b.txt", false, true));
		assert!(glob_match(b"[!abc].txt", b"d.txt", false, true));
		assert!(!glob_match(b"[a-c].txt", b"d.txt", false, true));
	}

	#[test]
	fn globs_pathspec_mode_wildcards_cross_slash() {
		// Default pathspec mode (`pathname = false`, git's fnmatch without FNM_PATHNAME): `*`/`?`/`[]`
		// cross `/`, and `**` is not special. Probed vs git 2.50.1.
		assert!(glob_match(b"*.rs", b"src/a.rs", false, false)); // `*` crosses `/`
		assert!(glob_match(b"src*", b"src/sub/b.rs", false, false));
		assert!(!glob_match(b"*.rs", b"src/a.rs", false, true)); // pathname mode: `*` stops at `/`
		assert!(glob_match(b"a?b", b"a/b", false, false)); // `?` crosses `/`
		assert!(!glob_match(b"a?b", b"a/b", false, true));
		assert!(glob_match(b"[a/]x", b"/x", false, false)); // `[]` may match `/`
		// `**` is not special without pathname mode: the literal `/` in `**/` must match, so `src/**/*.rs`
		// needs an intermediate directory (matches `src/sub/b.rs`, not `src/a.rs`).
		assert!(glob_match(b"src/**/*.rs", b"src/sub/b.rs", false, false));
		assert!(!glob_match(b"src/**/*.rs", b"src/a.rs", false, false));
		// With pathname mode `**` spans zero+ directories, so both match.
		assert!(glob_match(b"src/**/*.rs", b"src/sub/b.rs", false, true));
		assert!(glob_match(b"src/**/*.rs", b"src/a.rs", false, true));
	}

	#[test]
	fn globs_double_star_is_globstar_only_after_a_fixed_position() {
		// Pathname mode (`:(glob)`). A `**` run spans directories only when it is a whole path component:
		// preceded by a literal / `/` / start AND followed by `/` or end. Probed vs git 2.50.1 — `a**`
		// crosses `/`, but `?**` and `[x]**` (a wildcard before the run) do not.
		assert!(glob_match(b"a**", b"ab/f", false, true)); // literal before → globstar, crosses
		assert!(glob_match(b"a**", b"a", false, true));
		assert!(glob_match(b"ab***", b"ab/sub/g", false, true)); // 3 stars == 2, still globstar
		assert!(!glob_match(b"?**", b"ab/f", false, true)); // `?` before → NOT globstar
		assert!(glob_match(b"?**", b"ab", false, true)); // stays within the component
		assert!(!glob_match(b"[a]**", b"ab/f", false, true)); // class before → NOT globstar
		assert!(glob_match(b"[a]**", b"ab", false, true));
		// A `**` not at a component boundary (followed by a non-slash literal) is a plain `*`, so it
		// cannot cross `/`: `x**f` matches nothing under `x/`.
		assert!(!glob_match(b"x**f", b"x/f", false, true));
		// Leading / internal boundary globstars still span.
		assert!(glob_match(b"**", b"a/b/c", false, true));
		assert!(glob_match(b"**/g", b"a/sub/g", false, true));
		assert!(glob_match(b"a/**", b"a/b/c", false, true));
		// A wildcard EARLIER in the same component disqualifies the `**`, even when a literal sits right
		// before it: `*a**` stays within its component (`baa`), never spanning `/` (`ba/x`). Probed vs git.
		assert!(glob_match(b"*a**", b"baa", false, true));
		assert!(!glob_match(b"*a**", b"ba/x", false, true));
		assert!(glob_match(b"ba**", b"ba/x", false, true)); // all-literal prefix -> globstar
		// A trailing `**/` spans only COMPLETE directory components: `a**/` matches `a` (zero dirs) but not
		// `aa` (a partial leftover), unlike a trailing `a**` which matches the rest of the text entirely.
		assert!(glob_match(b"a**/", b"a", false, true));
		assert!(!glob_match(b"a**/", b"aa", false, true));
		assert!(!glob_match(b"a**/", b"a/x", false, true));
		assert!(glob_match(b"a**", b"aa", false, true));
	}

	#[test]
	fn globs_adversarial_star_pattern_is_not_exponential() {
		// `(*a){28}b` against 32 all-`a` bytes has no match, but the naive backtracking matcher explores
		// exponentially many splits. With the failure memo it returns immediately (this test would hang
		// for many seconds otherwise). Both pathname modes.
		let pat: Vec<u8> = std::iter::repeat_n(b"*a".as_slice(), 28)
			.flatten()
			.copied()
			.chain(std::iter::once(b'b'))
			.collect();
		let text = vec![b'a'; 32];
		assert!(!glob_match(&pat, &text, false, true));
		assert!(!glob_match(&pat, &text, false, false));
		// A matching variant still succeeds.
		assert!(glob_match(b"*a*a*a", b"aaa", false, true));
	}

	#[test]
	fn globs_class_backslash_escapes_member() {
		// A backslash inside a class escapes the next byte into a literal member, matching git wildmatch:
		// `[\-]` matches `-` only (not `\`, and not as a range). Probed vs git 2.50.1.
		assert!(glob_match(b"[\\-]", b"-", false, true));
		assert!(!glob_match(b"[\\-]", b"\\", false, true));
		// An escaped `]` is a member, not the terminator.
		assert!(glob_match(b"[a\\]b]", b"]", false, true));
		assert!(glob_match(b"[a\\]b]", b"a", false, true));
		assert!(!glob_match(b"[a\\]b]", b"c", false, true));
		// An escaped byte can be a range endpoint: `[\a-c]` is the range `a`..`c` (not the members
		// `a`, `-`, `c`), so `b` matches and `-` does not. Probed vs git 2.50.1.
		assert!(glob_match(b"[\\a-c]", b"b", false, true));
		assert!(!glob_match(b"[\\a-c]", b"-", false, true));
		// A leading `]` as a range low-end still works (`[]-a]` = `]`..`a`, so `_` at 0x5F matches).
		assert!(glob_match(b"[]-a]", b"_", false, true));
		assert!(!glob_match(b"[]-a]", b"b", false, true));
	}

	#[test]
	fn globs_unknown_posix_class_matches_nothing() {
		// An unknown POSIX class name is malformed: git aborts the whole match, so the pattern selects
		// nothing — even negated `[![:bogus:]]` matches nothing (probed vs git 2.50.1).
		assert!(!glob_match(b"a[a[:bogus:]]", b"aa", false, true));
		assert!(!glob_match(b"a[![:bogus:]]", b"ax", false, true));
		// A known class still works alongside the fix.
		assert!(glob_match(b"a[[:digit:]]", b"a5", false, true));
		assert!(!glob_match(b"a[[:digit:]]", b"ax", false, true));
	}

	#[test]
	fn globs_descending_range_matches_low_endpoint() {
		// A descending range `[c-a]` (lo > hi) is not empty: git matches its low (first) endpoint only, so
		// `a[c-a]` selects `ac` but not `aa`, and `[!c-a]` excludes just `c` (probed vs git 2.50.1).
		assert!(glob_match(b"a[c-a]", b"ac", false, true));
		assert!(!glob_match(b"a[c-a]", b"aa", false, true));
		assert!(!glob_match(b"a[!c-a]", b"ac", false, true));
		assert!(glob_match(b"a[!c-a]", b"aa", false, true));
	}

	#[test]
	fn globs_backslash_escapes() {
		// A backslash makes the next metacharacter literal (git's gitignore escapes, probed vs git 2.50.1).
		assert!(glob_match(b"a\\*", b"a*", false, true));
		assert!(!glob_match(b"a\\*", b"ax", false, true));
		assert!(glob_match(b"q\\?", b"q?", false, true));
		assert!(!glob_match(b"q\\?", b"qz", false, true));
		assert!(glob_match(b"br\\[ack", b"br[ack", false, true));
		// An escaped trailing space matches a literal space.
		assert!(glob_match(b"file\\ ", b"file ", false, true));
		assert!(!glob_match(b"file\\ ", b"file", false, true));
	}

	#[test]
	fn parses_escaped_trailing_space_and_metacharacters() {
		// The parser keeps an escaped trailing space and passes `\*` through to the glob engine as a
		// literal star (git's gitignore escapes).
		let root = parse("file\\ \na\\*\n", "");
		let one = std::slice::from_ref(&root);
		assert!(
			is_ignored("file ", false, one),
			"escaped trailing space kept"
		);
		assert!(!is_ignored("file", false, one));
		assert!(is_ignored("a*", false, one), "escaped star is literal");
		assert!(!is_ignored("ax", false, one));
	}

	#[test]
	fn globs_posix_classes() {
		// POSIX character classes inside `[...]` (git's wildmatch), probed vs git 2.50.1.
		assert!(glob_match(
			b"file[[:digit:]].txt",
			b"file1.txt",
			false,
			true
		));
		assert!(!glob_match(
			b"file[[:digit:]].txt",
			b"fileA.txt",
			false,
			true
		));
		assert!(glob_match(b"[[:alpha:]]", b"x", false, true));
		assert!(!glob_match(b"[[:alpha:]]", b"5", false, true));
		// A negated class and a class mixing POSIX with a literal member.
		assert!(glob_match(b"[![:digit:]]", b"a", false, true));
		assert!(!glob_match(b"[![:digit:]]", b"7", false, true));
		assert!(glob_match(b"[[:digit:]x]", b"x", false, true));
		// Under fold `[:upper:]` also matches a lowercase letter.
		assert!(glob_match(b"[[:upper:]]", b"a", true, true));
		assert!(!glob_match(b"[[:upper:]]", b"a", false, true));
	}

	#[test]
	fn globs_fold_ascii_case() {
		// Literals fold both directions under `fold` (git's core.ignoreCase), and not without it.
		assert!(glob_match(b"Dir", b"dir", true, true));
		assert!(glob_match(b"dir", b"DIR", true, true));
		assert!(!glob_match(b"Dir", b"dir", false, true));
		// Case-folded character classes: `[A-Z]` matches a lowercase letter and vice versa.
		assert!(glob_match(b"[A-Z].txt", b"b.txt", true, true));
		assert!(glob_match(b"[a-c].txt", b"B.txt", true, true));
		assert!(!glob_match(b"[a-c].txt", b"B.txt", false, true));
		// A non-letter still respects the class exactly (fold only swaps alpha).
		assert!(!glob_match(b"[a-c].txt", b"1.txt", true, true));
	}

	#[test]
	fn ignore_levels_and_negation() {
		let root = parse("*.log\n!keep.log\nbuild/\n", "");
		assert!(is_ignored("a.log", false, std::slice::from_ref(&root)));
		assert!(!is_ignored("keep.log", false, std::slice::from_ref(&root)));
		assert!(is_ignored("build", true, std::slice::from_ref(&root)));
		assert!(!is_ignored("build", false, std::slice::from_ref(&root))); // dir-only

		// A nested ignore file scoped to its directory.
		let nested = parse("secret.txt\n", "src");
		let stack = [root, nested];
		assert!(is_ignored("src/secret.txt", false, &stack));
		assert!(!is_ignored("secret.txt", false, &stack));
	}
}
