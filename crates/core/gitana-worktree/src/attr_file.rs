//! One attributes file (`.gitattributes`, `.git/info/attributes`, or a global or system file),
//! parsed as git's `attr.c` parses it.

use crate::AttrState;

/// The rules and macro definitions of one attributes file, tagged with the directory it governs,
/// relative to the working tree's root (`""` for the root and for the files outside the tree).
#[derive(Clone, Debug, Default)]
pub(crate) struct AttrFile {
	pub(crate) dir: Vec<u8>,
	pub(crate) rules: Vec<AttrRule>,
	pub(crate) macros: Vec<AttrMacro>,
}

/// One `pattern attr...` line.
#[derive(Clone, Debug)]
pub(crate) struct AttrRule {
	/// The glob, without a leading `/` or a trailing `/`.
	glob: Vec<u8>,
	/// The pattern had no `/`: it matches a file's name at any depth below the file's directory.
	basename: bool,
	/// The pattern ended with `/`: it matches only a directory, so never a file's attributes.
	dir_only: bool,
	pub(crate) attrs: Vec<(Vec<u8>, AttrState)>,
}

/// One `[attr]name attr...` macro definition.
#[derive(Clone, Debug)]
pub(crate) struct AttrMacro {
	pub(crate) name: Vec<u8>,
	pub(crate) attrs: Vec<(Vec<u8>, AttrState)>,
}

/// git's blank characters between a pattern and its attributes.
fn is_blank(byte: u8) -> bool {
	matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl AttrFile {
	/// Parse `text`, the attributes file governing `dir`. `macros` allows `[attr]` definitions,
	/// which git honours only in the root `.gitattributes`, `info/attributes`, and the global and
	/// system files. As git does, a negative pattern (`!x`) and a line naming an invalid
	/// attribute are ignored.
	pub(crate) fn parse(text: &[u8], dir: &[u8], macros: bool) -> AttrFile {
		let mut file = AttrFile {
			dir: dir.to_vec(),
			..AttrFile::default()
		};
		// As git does, a UTF-8 byte-order mark at the start isn't part of the first line.
		let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
		for line in text.split(|byte| *byte == b'\n') {
			// As git reads a line as a C string, a NUL ends it.
			let line = line.split(|byte| *byte == 0).next().unwrap_or(line);
			// git ignores a line of `ATTR_MAX_LINE_LENGTH` (2048) bytes or more.
			if line.len() >= 2048 {
				continue;
			}
			let start = line.iter().position(|byte| !is_blank(*byte));
			let Some(start) = start else {
				continue;
			};
			let line = &line[start..];
			if line.starts_with(b"#") {
				continue;
			}
			let (pattern, rest) = match line.strip_prefix(b"\"").and_then(c_unquote) {
				Some((pattern, rest)) => (pattern, rest),
				None => {
					let end = line
						.iter()
						.position(|byte| is_blank(*byte))
						.unwrap_or(line.len());
					(line[..end].to_vec(), &line[end..])
				}
			};
			let Some(attrs) = parse_attrs(rest) else {
				continue;
			};
			if let Some(name) = pattern.strip_prefix(b"[attr]") {
				if macros && valid_name(name) {
					file.macros.push(AttrMacro {
						name: name.to_vec(),
						attrs,
					});
				}
				continue;
			}
			if pattern.starts_with(b"!") || pattern.is_empty() {
				continue;
			}
			let dir_only = pattern.ends_with(b"/");
			let pattern = pattern.strip_suffix(b"/").unwrap_or(&pattern);
			let basename = !pattern.contains(&b'/');
			let glob = pattern.strip_prefix(b"/").unwrap_or(pattern).to_vec();
			file.rules.push(AttrRule {
				glob,
				basename,
				dir_only,
				attrs,
			});
		}
		file
	}
}

impl AttrRule {
	/// Whether this rule matches the file `rel`, its path relative to the rule's file's directory.
	pub(crate) fn matches(&self, rel: &[u8], fold: bool) -> bool {
		if self.dir_only {
			return false;
		}
		let subject = if self.basename {
			rel.rsplit(|byte| *byte == b'/').next().unwrap_or(rel)
		} else {
			rel
		};
		crate::ignore::pattern_match(&self.glob, subject, fold)
	}
}

/// The attributes after a pattern: `name` (set), `-name` (unset), `!name` (unspecified) and
/// `name=value`. As git's `parse_attr`, the name ends at an `=`, and a `-` or `!` before it wins over
/// any value (`-text=auto` is `text` unset). `None` if one names an invalid attribute, which makes
/// git ignore the line.
fn parse_attrs(rest: &[u8]) -> Option<Vec<(Vec<u8>, AttrState)>> {
	let mut attrs = Vec::new();
	for token in rest
		.split(|byte| is_blank(*byte))
		.filter(|token| !token.is_empty())
	{
		let (spelled, value) = match token.iter().position(|byte| *byte == b'=') {
			Some(equals) => (&token[..equals], Some(&token[equals + 1..])),
			None => (token, None),
		};
		let (name, state) = if let Some(name) = spelled.strip_prefix(b"-") {
			(name, AttrState::Unset)
		} else if let Some(name) = spelled.strip_prefix(b"!") {
			(name, AttrState::Unspecified)
		} else {
			match value {
				Some(value) => (spelled, AttrState::Value(value.to_vec())),
				None => (spelled, AttrState::Set),
			}
		};
		if !valid_name(name) {
			return None;
		}
		attrs.push((name.to_vec(), state));
	}
	Some(attrs)
}

/// git's `attr_name_valid`: letters, digits, `-`, `_` and `.`, not starting with `-`.
fn valid_name(name: &[u8]) -> bool {
	!name.is_empty()
		&& !name.starts_with(b"-")
		&& name
			.iter()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// git's `unquote_c_style` for a quoted pattern: `input` follows the opening `"`. The pattern and
/// what follows its closing quote, or `None` if the quoting is malformed (git then takes the line
/// unquoted).
fn c_unquote(input: &[u8]) -> Option<(Vec<u8>, &[u8])> {
	let mut out = Vec::new();
	let mut index = 0;
	while index < input.len() {
		let byte = input[index];
		index += 1;
		match byte {
			b'"' => return Some((out, &input[index..])),
			b'\\' => {
				let escape = *input.get(index)?;
				index += 1;
				let decoded = match escape {
					b'a' => 0x07,
					b'b' => 0x08,
					b'f' => 0x0c,
					b'n' => b'\n',
					b'r' => b'\r',
					b't' => b'\t',
					b'v' => 0x0b,
					b'\\' | b'"' => escape,
					b'0'..=b'3' => {
						let digits = input.get(index..index + 2)?;
						if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
							return None;
						}
						index += 2;
						((escape - b'0') << 6) | ((digits[0] - b'0') << 3) | (digits[1] - b'0')
					}
					_ => return None,
				};
				out.push(decoded);
			}
			_ => out.push(byte),
		}
	}
	None
}
