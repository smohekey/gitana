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
			if glob_match(pattern.glob.as_bytes(), subject.as_bytes(), false) {
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
	glob_match(pattern.glob.as_bytes(), glob_subject.as_bytes(), fold)
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

fn glob_match(pat: &[u8], text: &[u8], fold: bool) -> bool {
	match pat.first() {
		None => text.is_empty(),
		// A `**` is the special path-spanning glob only as a whole path component — `**` at the end of the
		// pattern (a trailing `/**`, matching everything under a prefix) or immediately followed by `/`
		// (a leading `**/` or an internal `/**/`, spanning zero or more directories). git treats any *other*
		// consecutive asterisks (e.g. `a/**b`) as regular single-`*` globs that do not cross a `/`; matching
		// git here keeps an untracked file from being mis-reported as ignored. So the recursive arm requires
		// the next byte to be absent or `/`; otherwise the first `*` falls through to the single-`*` arm below.
		Some(b'*') if pat.get(1) == Some(&b'*') && matches!(pat.get(2), None | Some(&b'/')) => {
			let after = match pat.get(2) {
				Some(&b'/') => &pat[3..],
				_ => &pat[2..],
			};
			if glob_match(after, text, fold) {
				return true;
			}
			for (i, &c) in text.iter().enumerate() {
				if c == b'/' && glob_match(after, &text[i + 1..], fold) {
					return true;
				}
			}
			after.is_empty()
		}
		Some(b'*') => {
			let after = &pat[1..];
			let mut i = 0;
			loop {
				if glob_match(after, &text[i..], fold) {
					return true;
				}
				if i >= text.len() || text[i] == b'/' {
					return false;
				}
				i += 1;
			}
		}
		Some(b'?') => match text.first() {
			Some(&c) if c != b'/' => glob_match(&pat[1..], &text[1..], fold),
			_ => false,
		},
		Some(b'[') => match_class(pat, text, fold),
		// A backslash escapes the next byte, disabling its glob meaning: `\*`, `\?`, `\[`, and `\ ` match
		// those characters literally (git's gitignore escapes). A trailing lone backslash matches a
		// literal backslash.
		Some(b'\\') => match pat.get(1) {
			Some(&escaped) => match text.first() {
				Some(&t) if byte_eq(t, escaped, fold) => glob_match(&pat[2..], &text[1..], fold),
				_ => false,
			},
			None => matches!(text.first(), Some(&b'\\')) && glob_match(&pat[1..], &text[1..], fold),
		},
		Some(&c) => match text.first() {
			Some(&t) if byte_eq(t, c, fold) => glob_match(&pat[1..], &text[1..], fold),
			_ => false,
		},
	}
}

fn match_class(pat: &[u8], text: &[u8], fold: bool) -> bool {
	let c = match text.first() {
		Some(&c) if c != b'/' => c,
		_ => return false,
	};
	let mut i = 1;
	let negate = matches!(pat.get(1), Some(b'!') | Some(b'^'));
	if negate {
		i = 2;
	}
	let mut matched = false;
	while i < pat.len() && pat[i] != b']' {
		// A POSIX character class `[:name:]` (e.g. `[[:digit:]]`) — recognise it before the `]` in its
		// closing `:]` is mistaken for the end of the outer class.
		if pat[i] == b'['
			&& pat.get(i + 1) == Some(&b':')
			&& let Some(rel) = pat[i + 2..].windows(2).position(|w| w == b":]")
		{
			if posix_class_match(&pat[i + 2..i + 2 + rel], c, fold) {
				matched = true;
			}
			i += 2 + rel + 2; // skip `[:` + name + `:]`
			continue;
		}
		if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
			if in_range(pat[i], pat[i + 2], c, fold) {
				matched = true;
			}
			i += 3;
		} else {
			if byte_eq(pat[i], c, fold) {
				matched = true;
			}
			i += 1;
		}
	}
	if i >= pat.len() {
		return false; // unterminated class
	}
	if matched != negate {
		glob_match(&pat[i + 1..], &text[1..], fold)
	} else {
		false
	}
}

/// Whether byte `c` is a member of the POSIX character class `[:name:]` (git's wildmatch supports these
/// in gitignore-style patterns). Under `fold`, an alpha `c` also tries its opposite case, so `[:upper:]`
/// matches a lowercase letter and vice versa. An unknown class name matches nothing.
fn posix_class_match(name: &[u8], c: u8, fold: bool) -> bool {
	let member = |c: u8| match name {
		b"alpha" => c.is_ascii_alphabetic(),
		b"digit" => c.is_ascii_digit(),
		b"alnum" => c.is_ascii_alphanumeric(),
		b"upper" => c.is_ascii_uppercase(),
		b"lower" => c.is_ascii_lowercase(),
		b"space" => matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
		b"blank" => matches!(c, b' ' | b'\t'),
		b"punct" => c.is_ascii_punctuation(),
		b"cntrl" => c.is_ascii_control(),
		b"xdigit" => c.is_ascii_hexdigit(),
		b"graph" => c.is_ascii_graphic(),
		b"print" => c.is_ascii_graphic() || c == b' ',
		_ => false,
	};
	member(c) || (fold && c.is_ascii_alphabetic() && member(c ^ 0x20))
}

/// Whether byte `c` falls in the class range `lo..=hi`, folding ASCII case when `fold`: an alpha `c`
/// that misses the range is retried in its opposite case, so `[A-Z]` matches `a` and `[a-z]` matches
/// `A` (git's `core.ignoreCase` wildmatch case-fold).
fn in_range(lo: u8, hi: u8, c: u8, fold: bool) -> bool {
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
		assert!(glob_match(b"*.log", b"a.log", false));
		assert!(!glob_match(b"*.log", b"a.txt", false));
		assert!(!glob_match(b"*.log", b"a/b.log", false)); // * does not cross '/'
		assert!(glob_match(b"**/*.log", b"a/b/c.log", false));
		assert!(glob_match(b"build/**", b"build/x/y", false));
		assert!(glob_match(b"a/**/b", b"a/x/y/b", false));
		assert!(glob_match(b"a/**/b", b"a/b", false));
		// `**` spans directories only as a whole path component. `a/**b` is *not* such a component, so git
		// treats it as regular asterisks that do not cross '/': it matches `a/xb` but not `a/a/b` (verified
		// against stock git, which reports `a/a/b` untracked). A mis-recursive match here would let removal
		// delete an untracked file it believed ignored.
		assert!(glob_match(b"a/**b", b"a/xb", false));
		assert!(!glob_match(b"a/**b", b"a/a/b", false));
		assert!(glob_match(b"file?", b"file1", false));
		assert!(glob_match(b"[abc].txt", b"b.txt", false));
		assert!(glob_match(b"[!abc].txt", b"d.txt", false));
		assert!(!glob_match(b"[a-c].txt", b"d.txt", false));
	}

	#[test]
	fn globs_backslash_escapes() {
		// A backslash makes the next metacharacter literal (git's gitignore escapes, probed vs git 2.50.1).
		assert!(glob_match(b"a\\*", b"a*", false));
		assert!(!glob_match(b"a\\*", b"ax", false));
		assert!(glob_match(b"q\\?", b"q?", false));
		assert!(!glob_match(b"q\\?", b"qz", false));
		assert!(glob_match(b"br\\[ack", b"br[ack", false));
		// An escaped trailing space matches a literal space.
		assert!(glob_match(b"file\\ ", b"file ", false));
		assert!(!glob_match(b"file\\ ", b"file", false));
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
		assert!(glob_match(b"file[[:digit:]].txt", b"file1.txt", false));
		assert!(!glob_match(b"file[[:digit:]].txt", b"fileA.txt", false));
		assert!(glob_match(b"[[:alpha:]]", b"x", false));
		assert!(!glob_match(b"[[:alpha:]]", b"5", false));
		// A negated class and a class mixing POSIX with a literal member.
		assert!(glob_match(b"[![:digit:]]", b"a", false));
		assert!(!glob_match(b"[![:digit:]]", b"7", false));
		assert!(glob_match(b"[[:digit:]x]", b"x", false));
		// Under fold `[:upper:]` also matches a lowercase letter.
		assert!(glob_match(b"[[:upper:]]", b"a", true));
		assert!(!glob_match(b"[[:upper:]]", b"a", false));
	}

	#[test]
	fn globs_fold_ascii_case() {
		// Literals fold both directions under `fold` (git's core.ignoreCase), and not without it.
		assert!(glob_match(b"Dir", b"dir", true));
		assert!(glob_match(b"dir", b"DIR", true));
		assert!(!glob_match(b"Dir", b"dir", false));
		// Case-folded character classes: `[A-Z]` matches a lowercase letter and vice versa.
		assert!(glob_match(b"[A-Z].txt", b"b.txt", true));
		assert!(glob_match(b"[a-c].txt", b"B.txt", true));
		assert!(!glob_match(b"[a-c].txt", b"B.txt", false));
		// A non-letter still respects the class exactly (fold only swaps alpha).
		assert!(!glob_match(b"[a-c].txt", b"1.txt", true));
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
