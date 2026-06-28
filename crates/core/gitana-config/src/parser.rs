//! A git-config parser following the syntax in `git help config`.
//!
//! Handles `[section]` / `[section "subsection"]` / deprecated `[section.subsection]`,
//! case folding (section/name lower-cased, subsection verbatim), `name`/`name = value`
//! with whitespace trimming, double-quoting, the `\n` `\t` `\b` `\"` `\\` escapes,
//! trailing-backslash line continuation, and `#`/`;` comments (literal inside quotes).
//!
//! Single-pass with one char of lookahead — it streams a `Peekable<Chars>` and never
//! rewinds, so the source is not materialised into a buffer.

use std::iter::Peekable;
use std::str::Chars;

use crate::ConfigError;
use crate::config::{GitConfig, Variable};

pub(crate) fn parse(text: &str) -> Result<GitConfig, ConfigError> {
	let mut parser = Parser {
		chars: text.chars().peekable(),
		section: None,
	};
	let mut variables = Vec::new();
	loop {
		parser.skip_blank();
		let Some(c) = parser.peek() else { break };
		if c == '[' {
			parser.parse_section_header()?;
		} else if c.is_ascii_alphabetic() {
			let (name, value) = parser.parse_variable()?;
			let (section, subsection) = parser
				.section
				.clone()
				.ok_or_else(|| ConfigError::Parse("variable before any section".to_owned()))?;
			variables.push(Variable {
				section,
				subsection,
				name,
				value,
			});
		} else {
			return Err(ConfigError::Parse(format!("unexpected character {c:?}")));
		}
	}
	Ok(GitConfig::from_variables(variables))
}

struct Parser<'a> {
	chars: Peekable<Chars<'a>>,
	section: Option<(String, Option<String>)>,
}

impl Parser<'_> {
	fn peek(&mut self) -> Option<char> {
		self.chars.peek().copied()
	}

	fn bump(&mut self) -> Option<char> {
		self.chars.next()
	}

	/// Skip blank lines, leading whitespace, and whole-line comments.
	fn skip_blank(&mut self) {
		while let Some(c) = self.peek() {
			match c {
				' ' | '\t' | '\n' | '\r' => {
					self.bump();
				}
				'#' | ';' => self.skip_to_newline(),
				_ => break,
			}
		}
	}

	fn skip_to_newline(&mut self) {
		while let Some(c) = self.bump() {
			if c == '\n' {
				break;
			}
		}
	}

	fn skip_spaces(&mut self) {
		while matches!(self.peek(), Some(' ' | '\t')) {
			self.bump();
		}
	}

	fn parse_section_header(&mut self) -> Result<(), ConfigError> {
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
		self.section = Some((section.to_ascii_lowercase(), subsection));
		Ok(())
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

	fn parse_variable(&mut self) -> Result<(String, Option<String>), ConfigError> {
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
				self.skip_to_newline();
				None
			}
			Some('\n') | None => {
				self.bump();
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

	/// Parse a value: quoting, escapes, continuation, comment, trailing-space trim.
	fn parse_value(&mut self) -> Result<String, ConfigError> {
		self.skip_spaces(); // discard whitespace between '=' and the value
		let mut value = String::new();
		let mut quoted = false;
		let mut pending_spaces = 0usize;

		while let Some(c) = self.bump() {
			match c {
				'\n' if !quoted => break,
				'\n' => return Err(ConfigError::Parse("unterminated quote".to_owned())),
				' ' | '\t' if !quoted => {
					pending_spaces += 1;
					continue;
				}
				'#' | ';' if !quoted => {
					self.skip_to_newline();
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
		}
		Ok(value)
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
	}

	#[test]
	fn boolean_true_name_only_and_multivalue() {
		let text = "[core]\n\tbare\n[remote \"o\"]\n\tpush = a\n\tpush = b\n";
		let config = parse(text).unwrap();
		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(true));
		assert_eq!(config.get_all("remote", Some("o"), "push"), vec!["a", "b"]);
	}

	#[test]
	fn deprecated_dotted_subsection() {
		let config = parse("[a.B]\n\tx = 1\n").unwrap();
		// Subsection lower-cased per the deprecated form.
		assert_eq!(config.get_string("a", Some("b"), "x"), Some("1"));
	}
}
