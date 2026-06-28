//! A from-scratch `.gitignore` matcher.
//!
//! Supports per-directory ignore files, `#` comments, `!` negation, trailing-`/`
//! directory-only patterns, leading-`/` and internal-`/` anchoring, basename
//! matching for unanchored patterns, and the `*` `?` `[...]` `**` globs. Last
//! matching pattern (deepest file, then last line) wins. Not handled:
//! `.git/info/exclude`, the global excludes file, and `\` escapes of `#`/`!`.

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
		let line = raw.trim_end();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let (negated, body) = match line.strip_prefix('!') {
			Some(rest) => (true, rest),
			None => (false, line),
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
			if glob_match(pattern.glob.as_bytes(), subject.as_bytes()) {
				ignored = !pattern.negated;
			}
		}
	}
	ignored
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

fn glob_match(pat: &[u8], text: &[u8]) -> bool {
	match pat.first() {
		None => text.is_empty(),
		Some(b'*') if pat.get(1) == Some(&b'*') => {
			let after = match pat.get(2) {
				Some(&b'/') => &pat[3..],
				_ => &pat[2..],
			};
			if glob_match(after, text) {
				return true;
			}
			for (i, &c) in text.iter().enumerate() {
				if c == b'/' && glob_match(after, &text[i + 1..]) {
					return true;
				}
			}
			after.is_empty()
		}
		Some(b'*') => {
			let after = &pat[1..];
			let mut i = 0;
			loop {
				if glob_match(after, &text[i..]) {
					return true;
				}
				if i >= text.len() || text[i] == b'/' {
					return false;
				}
				i += 1;
			}
		}
		Some(b'?') => match text.first() {
			Some(&c) if c != b'/' => glob_match(&pat[1..], &text[1..]),
			_ => false,
		},
		Some(b'[') => match_class(pat, text),
		Some(&c) => match text.first() {
			Some(&t) if t == c => glob_match(&pat[1..], &text[1..]),
			_ => false,
		},
	}
}

fn match_class(pat: &[u8], text: &[u8]) -> bool {
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
		if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
			if pat[i] <= c && c <= pat[i + 2] {
				matched = true;
			}
			i += 3;
		} else {
			if pat[i] == c {
				matched = true;
			}
			i += 1;
		}
	}
	if i >= pat.len() {
		return false; // unterminated class
	}
	if matched != negate {
		glob_match(&pat[i + 1..], &text[1..])
	} else {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn globs() {
		assert!(glob_match(b"*.log", b"a.log"));
		assert!(!glob_match(b"*.log", b"a.txt"));
		assert!(!glob_match(b"*.log", b"a/b.log")); // * does not cross '/'
		assert!(glob_match(b"**/*.log", b"a/b/c.log"));
		assert!(glob_match(b"build/**", b"build/x/y"));
		assert!(glob_match(b"a/**/b", b"a/x/y/b"));
		assert!(glob_match(b"a/**/b", b"a/b"));
		assert!(glob_match(b"file?", b"file1"));
		assert!(glob_match(b"[abc].txt", b"b.txt"));
		assert!(glob_match(b"[!abc].txt", b"d.txt"));
		assert!(!glob_match(b"[a-c].txt", b"d.txt"));
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
