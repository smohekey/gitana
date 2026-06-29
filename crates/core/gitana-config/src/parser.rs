//! A git-config parser following the syntax in `git help config`.
//!
//! Handles `[section]` / `[section "subsection"]` / deprecated `[section.subsection]`,
//! case folding (section/name lower-cased, subsection verbatim), `name`/`name = value`
//! with whitespace trimming, double-quoting, the `\n` `\t` `\b` `\"` `\\` escapes,
//! trailing-backslash line continuation, and `#`/`;` comments (literal inside quotes).
//!
//! Lossless: every byte of the input lands in exactly one [`Element`], so concatenating the
//! elements' raw text reproduces the source verbatim. Each section/variable element owns its
//! whole physical line(s) (leading indentation through the terminating newline); blank and
//! comment lines, and any inter-line gaps, become `Filler`. A variable also records the byte
//! span of its value text within its raw line, so a writer can replace just the value in place.
//!
//! Byte-cursor over the source `&str` (peeking via `text[pos..].chars().next()`), so spans are
//! recordable; the source is never copied except into each element's retained raw text.

use std::ops::Range;

use crate::ConfigError;
use crate::config::{Element, GitConfig, Section, Variable};

pub(crate) fn parse(text: &str) -> Result<GitConfig, ConfigError> {
	let mut parser = Parser { text, pos: 0 };
	let mut elements = Vec::new();
	// The section/subsection in scope, denormalised onto each variable so lookups need no
	// back-reference to the header element.
	let mut section: Option<(String, Option<String>)> = None;

	while parser.pos < text.len() {
		let line_start = parser.pos;
		// Look past leading horizontal whitespace to classify the line; the raw still starts at
		// `line_start` so the indentation is preserved.
		let mut probe = parser.pos;
		while matches!(text.as_bytes().get(probe), Some(b' ' | b'\t')) {
			probe += 1;
		}
		match text[probe..].chars().next() {
			None | Some('\n') | Some('\r') | Some('#') | Some(';') => {
				// Blank line or whole-line comment.
				parser.consume_line();
				elements.push(Element::Filler(text[line_start..parser.pos].to_owned()));
			}
			Some('[') => {
				parser.pos = probe;
				let (sec, sub) = parser.parse_section_header()?;
				parser.consume_line();
				let raw = text[line_start..parser.pos].to_owned();
				section = Some((sec.clone(), sub.clone()));
				elements.push(Element::Section(Section {
					section: sec,
					subsection: sub,
					raw,
				}));
			}
			Some(c) if c.is_ascii_alphabetic() => {
				parser.pos = probe;
				let (name, value) = parser.parse_variable()?;
				let (sec, sub) = section
					.clone()
					.ok_or_else(|| ConfigError::Parse("variable before any section".to_owned()))?;
				let raw = text[line_start..parser.pos].to_owned();
				let (value, value_span) = match value {
					Some((v, span)) => (
						Some(v),
						Some((span.start - line_start)..(span.end - line_start)),
					),
					None => (None, None),
				};
				elements.push(Element::Variable(Variable {
					section: sec,
					subsection: sub,
					name,
					value,
					raw,
					value_span,
				}));
			}
			Some(c) => return Err(ConfigError::Parse(format!("unexpected character {c:?}"))),
		}
	}

	Ok(GitConfig::from_elements(elements))
}

struct Parser<'a> {
	text: &'a str,
	pos: usize,
}

impl Parser<'_> {
	fn peek(&self) -> Option<char> {
		self.text[self.pos..].chars().next()
	}

	fn bump(&mut self) -> Option<char> {
		let c = self.peek()?;
		self.pos += c.len_utf8();
		Some(c)
	}

	fn skip_spaces(&mut self) {
		while matches!(self.peek(), Some(' ' | '\t')) {
			self.bump();
		}
	}

	/// Advance past the next newline (inclusive), or to end of input.
	fn consume_line(&mut self) {
		while let Some(c) = self.bump() {
			if c == '\n' {
				break;
			}
		}
	}

	/// Parse `[section]` / `[section "subsection"]` / deprecated `[section.subsection]`, stopping
	/// right after the closing `]`. Returns the lower-cased section and the (verbatim) subsection.
	fn parse_section_header(&mut self) -> Result<(String, Option<String>), ConfigError> {
		self.bump(); // consume '['
		let mut section = String::new();
		while let Some(c) = self.peek() {
			if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
				section.push(c);
				self.bump();
			} else {
				break;
			}
		}
		self.skip_spaces();

		let subsection = if self.peek() == Some('"') {
			Some(self.parse_quoted_subsection()?)
		} else if let Some((head, tail)) = section.split_once('.') {
			// Deprecated `[section.subsection]`: subsection lower-cased.
			let sub = tail.to_ascii_lowercase();
			section = head.to_owned();
			Some(sub)
		} else {
			None
		};

		self.skip_spaces();
		if self.bump() != Some(']') {
			return Err(ConfigError::Parse("expected ']'".to_owned()));
		}
		Ok((section.to_ascii_lowercase(), subsection))
	}

	fn parse_quoted_subsection(&mut self) -> Result<String, ConfigError> {
		self.bump(); // consume opening '"'
		let mut out = String::new();
		loop {
			match self.bump() {
				None | Some('\n') => {
					return Err(ConfigError::Parse("unterminated subsection".to_owned()));
				}
				Some('"') => return Ok(out),
				Some('\\') => match self.bump() {
					Some('"') => out.push('"'),
					Some('\\') => out.push('\\'),
					// Backslashes before other characters are dropped.
					Some(other) => out.push(other),
					None => return Err(ConfigError::Parse("unterminated subsection".to_owned())),
				},
				Some(other) => out.push(other),
			}
		}
	}

	/// Parse a variable line, consuming through its terminating newline (including any
	/// continuation lines). Returns the lower-cased name and, for `name = value`, the parsed value
	/// plus the absolute byte span of its value text in the source (`None` for a bare variable).
	#[allow(clippy::type_complexity)]
	fn parse_variable(&mut self) -> Result<(String, Option<(String, Range<usize>)>), ConfigError> {
		let mut name = String::new();
		while let Some(c) = self.peek() {
			if c.is_ascii_alphanumeric() || c == '-' {
				name.push(c);
				self.bump();
			} else {
				break;
			}
		}
		self.skip_spaces();

		let value = match self.peek() {
			Some('=') => {
				self.bump();
				Some(self.parse_value()?)
			}
			Some('#' | ';') => {
				self.consume_line();
				None
			}
			Some('\n') | None => {
				self.bump();
				None
			}
			// A bare variable ending in CRLF (or a lone CR); consume to end of line.
			Some('\r') => {
				self.consume_line();
				None
			}
			Some(c) => {
				return Err(ConfigError::Parse(format!(
					"expected '=' after name, got {c:?}"
				)));
			}
		};
		Ok((name.to_ascii_lowercase(), value))
	}

	/// Parse a value: quoting, escapes, continuation, comment, trailing-space trim. Consumes the
	/// terminating newline. Returns the decoded value and the absolute byte span its text occupies
	/// in the source (trailing whitespace and any inline comment excluded).
	fn parse_value(&mut self) -> Result<(String, Range<usize>), ConfigError> {
		self.skip_spaces(); // discard whitespace between '=' and the value
		let value_start = self.pos;
		let mut value_end = self.pos;
		let mut value = String::new();
		let mut quoted = false;
		let mut pending_spaces = 0usize;

		while let Some(c) = self.bump() {
			match c {
				'\n' if !quoted => break,
				'\n' => return Err(ConfigError::Parse("unterminated quote".to_owned())),
				// CRLF line ending: the `\r` is not part of the value. Consume the `\n` and stop.
				'\r' if !quoted && self.peek() == Some('\n') => {
					self.bump();
					break;
				}
				' ' | '\t' if !quoted => {
					pending_spaces += 1;
					continue;
				}
				'#' | ';' if !quoted => {
					self.consume_line();
					break;
				}
				_ => {}
			}
			for _ in 0..pending_spaces {
				value.push(' ');
			}
			pending_spaces = 0;
			match c {
				'\\' => match self.bump() {
					Some('\n') => {} // line continuation
					// CRLF line continuation: drop the `\r\n`.
					Some('\r') if self.peek() == Some('\n') => {
						self.bump();
					}
					Some('"') => value.push('"'),
					Some('\\') => value.push('\\'),
					Some('n') => value.push('\n'),
					Some('t') => value.push('\t'),
					Some('b') => value.push('\u{8}'),
					other => {
						return Err(ConfigError::Parse(format!(
							"invalid escape \\{}",
							other.unwrap_or(' ')
						)));
					}
				},
				'"' => quoted = !quoted,
				other => value.push(other),
			}
			// Everything up to here contributes to the value; trailing spaces (`pending_spaces`)
			// and an inline comment never reach this point, so they fall outside the span.
			value_end = self.pos;
		}
		Ok((value, value_start..value_end))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_sections_subsections_and_values() {
		let text = r#"
            [core]
                repositoryformatversion = 1
                bare = false
            [remote "origin"]
                url = https://example.com/r.git
            [extensions]
                objectformat = sha256
        "#;
		let config = parse(text).unwrap();
		assert_eq!(
			config
				.get_int("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);
		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(false));
		assert_eq!(
			config.get_string("remote", Some("origin"), "url"),
			Some("https://example.com/r.git")
		);
		assert_eq!(
			config.get_string("extensions", None, "objectformat"),
			Some("sha256")
		);
		// Lossless: the source round-trips byte-for-byte.
		assert_eq!(config.render(), text);
	}

	#[test]
	fn quotes_escapes_comments_and_continuation() {
		let text = concat!(
			"[user]\n",
			"\tname = \"  spaced  \"  ; trailing comment\n",
			"\ttabbed = a\\tb\n",
			"\thash = \"a#b;c\"\n",
			"\tcont = one\\\n",
			"two\n",
		);
		let config = parse(text).unwrap();
		assert_eq!(config.get_string("user", None, "name"), Some("  spaced  "));
		assert_eq!(config.get_string("user", None, "tabbed"), Some("a\tb"));
		assert_eq!(config.get_string("user", None, "hash"), Some("a#b;c"));
		assert_eq!(config.get_string("user", None, "cont"), Some("onetwo"));
		assert_eq!(config.render(), text);
	}

	#[test]
	fn boolean_true_name_only_and_multivalue() {
		let text = "[core]\n\tbare\n[remote \"o\"]\n\tpush = a\n\tpush = b\n";
		let config = parse(text).unwrap();
		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(true));
		assert_eq!(config.get_all("remote", Some("o"), "push"), vec!["a", "b"]);
		assert_eq!(config.render(), text);
	}

	#[test]
	fn deprecated_dotted_subsection() {
		let text = "[a.B]\n\tx = 1\n";
		let config = parse(text).unwrap();
		// Subsection lower-cased per the deprecated form.
		assert_eq!(config.get_string("a", Some("b"), "x"), Some("1"));
		// ...but the original header form is preserved on render.
		assert_eq!(config.render(), text);
	}

	#[test]
	fn preserves_comments_blanks_and_indentation() {
		let text = concat!(
			"# leading comment\n",
			"\n",
			"[core]   ; inline section comment\n",
			"    repositoryformatversion = 1   # trailing\n",
			"\n",
			"; dangling comment\n",
		);
		assert_eq!(parse(text).unwrap().render(), text);
	}

	#[test]
	fn final_line_without_newline_round_trips() {
		let text = "[core]\n\tbare = true";
		let config = parse(text).unwrap();
		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(true));
		assert_eq!(config.render(), text);
	}

	#[test]
	fn crlf_line_endings_round_trip_without_stray_carriage_returns() {
		let text = "[user]\r\n\tname = Alice\r\n\tbare\r\n\tcont = one\\\r\ntwo\r\n";
		let config = parse(text).unwrap();
		// The `\r` is part of the line ending, not the value.
		assert_eq!(config.get_string("user", None, "name"), Some("Alice"));
		assert_eq!(config.get_bool("user", None, "bare").unwrap(), Some(true));
		assert_eq!(config.get_string("user", None, "cont"), Some("onetwo"));
		// And the CRLF endings survive a round-trip.
		assert_eq!(config.render(), text);
	}
}
