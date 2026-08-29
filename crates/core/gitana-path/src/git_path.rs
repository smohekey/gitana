use std::{borrow::Borrow, fmt, ops::Deref};

use super::{GitPathComponent, GitPathError};

/// A canonical repository-relative Git path stored as its exact on-disk bytes.
///
/// `/` is Git's separator on every host. The empty value denotes the worktree
/// root; every non-empty value consists only of validated path components.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitPath(Vec<u8>);

impl GitPath {
	/// The worktree root.
	#[must_use]
	pub const fn root() -> Self {
		Self(Vec::new())
	}

	/// Validates and owns raw Git pathname bytes.
	pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, GitPathError> {
		let bytes = bytes.into();
		validate(&bytes)?;
		Ok(Self(bytes))
	}

	/// Validates a UTF-8 repository-relative path without changing its bytes.
	pub fn from_utf8(path: impl AsRef<str>) -> Result<Self, GitPathError> {
		Self::from_bytes(path.as_ref().as_bytes().to_vec())
	}

	/// Returns the exact Git pathname bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Consumes the path and returns its exact Git pathname bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}

	/// Returns the path as UTF-8 when its bytes are valid UTF-8.
	#[must_use]
	pub fn as_utf8(&self) -> Option<&str> {
		std::str::from_utf8(&self.0).ok()
	}

	/// Whether this path denotes the worktree root.
	#[must_use]
	pub fn is_root(&self) -> bool {
		self.0.is_empty()
	}

	/// Appends one validated component.
	#[must_use]
	pub fn join(&self, component: &GitPathComponent) -> Self {
		let mut bytes = Vec::with_capacity(
			self.0.len() + usize::from(!self.0.is_empty()) + component.as_bytes().len(),
		);
		bytes.extend_from_slice(&self.0);
		if !bytes.is_empty() {
			bytes.push(b'/');
		}
		bytes.extend_from_slice(component.as_bytes());
		Self(bytes)
	}

	/// Appends another canonical relative path.
	#[must_use]
	pub fn join_path(&self, tail: &Self) -> Self {
		if self.is_root() {
			return tail.clone();
		}
		if tail.is_root() {
			return self.clone();
		}
		let mut bytes = Vec::with_capacity(self.0.len() + 1 + tail.0.len());
		bytes.extend_from_slice(&self.0);
		bytes.push(b'/');
		bytes.extend_from_slice(&tail.0);
		Self(bytes)
	}

	/// Builds a one-component path without decoding or revalidating the component.
	#[must_use]
	pub fn from_component(component: &GitPathComponent) -> Self {
		Self(component.as_bytes().to_vec())
	}

	/// Whether this path is equal to `base` or lies below it at a component boundary.
	#[must_use]
	pub fn is_at_or_below(&self, base: &Self) -> bool {
		base.is_root()
			|| self == base
			|| (self.0.len() > base.0.len()
				&& self.0.starts_with(&base.0)
				&& self.0[base.0.len()] == b'/')
	}

	/// Whether this path lies strictly below `base` at a component boundary.
	#[must_use]
	pub fn is_below(&self, base: &Self) -> bool {
		self != base && self.is_at_or_below(base)
	}

	/// Whether this path lies strictly below `base`, optionally folding ASCII case.
	#[must_use]
	pub fn is_below_with_ascii_case(&self, base: &Self, fold: bool) -> bool {
		if base.is_root() {
			return !self.is_root();
		}
		self.0.len() > base.0.len()
			&& self.0[base.0.len()] == b'/'
			&& if fold {
				self.0[..base.0.len()].eq_ignore_ascii_case(&base.0)
			} else {
				self.0.starts_with(&base.0)
			}
	}

	/// Returns this path relative to `base` when it is at or below that base.
	#[must_use]
	pub fn relative_to(&self, base: &Self) -> Option<Self> {
		if self == base {
			return Some(Self::root());
		}
		if base.is_root() {
			return Some(self.clone());
		}
		self
			.is_below(base)
			.then(|| Self(self.0[base.0.len() + 1..].to_vec()))
	}

	/// Renders this path after composing raw display-only bytes around it.
	///
	/// The affixes participate in the same C-style quoting as the pathname. This is
	/// used for Git presentation names such as `a/path` and `directory/`, whose
	/// prefixes or trailing slash must remain inside any surrounding quotes.
	#[must_use]
	pub fn quote_with_affixes(&self, prefix: &[u8], suffix: &[u8]) -> String {
		String::from_utf8(self.render_with_affixes(prefix, suffix, true))
			.expect("safe Git quoting is ASCII")
	}

	/// Renders this path with raw display affixes under Git's `core.quotePath` policy.
	///
	/// When `quote_non_ascii` is false, printable bytes at or above `0x80` are kept verbatim. Control
	/// bytes, quotes, and backslashes remain C-style quoted in either mode.
	#[must_use]
	pub fn render_with_affixes(
		&self,
		prefix: &[u8],
		suffix: &[u8],
		quote_non_ascii: bool,
	) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(prefix.len() + self.0.len() + suffix.len());
		bytes.extend_from_slice(prefix);
		bytes.extend_from_slice(&self.0);
		bytes.extend_from_slice(suffix);
		crate::render_bytes(&bytes, quote_non_ascii)
	}

	/// Returns the exact bytes with ASCII letters folded to lowercase.
	#[must_use]
	pub fn ascii_folded(&self) -> Vec<u8> {
		self.0.iter().map(u8::to_ascii_lowercase).collect()
	}

	/// Compares exact path bytes, optionally folding ASCII case.
	#[must_use]
	pub fn eq_with_ascii_case(&self, other: &Self, fold: bool) -> bool {
		if fold {
			self.0.eq_ignore_ascii_case(&other.0)
		} else {
			self == other
		}
	}

	/// Returns the parent, or `None` for the root.
	#[must_use]
	pub fn parent(&self) -> Option<Self> {
		if self.0.is_empty() {
			return None;
		}
		let parent = self
			.0
			.iter()
			.rposition(|byte| *byte == b'/')
			.map_or(&[][..], |separator| &self.0[..separator]);
		Some(Self(parent.to_vec()))
	}

	/// Returns the final component, or `None` for the root.
	#[must_use]
	pub fn file_name(&self) -> Option<GitPathComponent> {
		if self.0.is_empty() {
			return None;
		}
		let start = self
			.0
			.iter()
			.rposition(|byte| *byte == b'/')
			.map_or(0, |separator| separator + 1);
		Some(GitPathComponent::from_validated(self.0[start..].to_vec()))
	}

	/// Iterates over path components without decoding them.
	pub fn components(&self) -> impl Iterator<Item = &[u8]> {
		self
			.0
			.split(|byte| *byte == b'/')
			.filter(|part| !part.is_empty())
	}

	/// Returns strict ancestors from the root-most directory to the immediate parent.
	#[must_use]
	pub fn strict_ancestors(&self) -> Vec<Self> {
		self
			.0
			.iter()
			.enumerate()
			.filter(|(_, byte)| **byte == b'/')
			.map(|(separator, _)| Self(self.0[..separator].to_vec()))
			.collect()
	}
}

impl AsRef<[u8]> for GitPath {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Borrow<[u8]> for GitPath {
	fn borrow(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Deref for GitPath {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		self.as_bytes()
	}
}

impl PartialEq<str> for GitPath {
	fn eq(&self, other: &str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<&str> for GitPath {
	fn eq(&self, other: &&str) -> bool {
		self == *other
	}
}

impl PartialEq<String> for GitPath {
	fn eq(&self, other: &String) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<GitPath> for String {
	fn eq(&self, other: &GitPath) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<GitPath> for str {
	fn eq(&self, other: &GitPath) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl fmt::Display for GitPath {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_human(self.as_bytes(), formatter)
	}
}

impl fmt::Debug for GitPath {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_tuple("GitPath")
			.field(&Escaped(&self.0))
			.finish()
	}
}

struct Escaped<'a>(&'a [u8]);

impl fmt::Debug for Escaped<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_quoted(self.0, formatter)
	}
}

fn validate(bytes: &[u8]) -> Result<(), GitPathError> {
	if bytes.contains(&0) {
		return Err(GitPathError::Nul);
	}
	if bytes.is_empty() {
		return Ok(());
	}
	if bytes.first() == Some(&b'/') || bytes.last() == Some(&b'/') {
		return Err(GitPathError::EmptyComponent);
	}
	for component in bytes.split(|byte| *byte == b'/') {
		if component.is_empty() {
			return Err(GitPathError::EmptyComponent);
		}
		if component == b"." || component == b".." {
			return Err(GitPathError::Traversal);
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn preserves_arbitrary_non_nul_bytes() {
		let path = GitPath::from_bytes(b"src/\xff.rs".to_vec()).unwrap();
		assert_eq!(path.as_bytes(), b"src/\xff.rs");
		assert_eq!(path.as_utf8(), None);
	}

	#[test]
	fn rejects_non_canonical_paths() {
		for bytes in [b"/a".as_slice(), b"a/".as_slice(), b"a//b".as_slice()] {
			assert_eq!(
				GitPath::from_bytes(bytes.to_vec()).unwrap_err(),
				GitPathError::EmptyComponent
			);
		}
		for bytes in [b".".as_slice(), b"a/../b".as_slice()] {
			assert_eq!(
				GitPath::from_bytes(bytes.to_vec()).unwrap_err(),
				GitPathError::Traversal
			);
		}
		assert_eq!(
			GitPath::from_bytes(b"a\0b".to_vec()).unwrap_err(),
			GitPathError::Nul
		);
	}

	#[test]
	fn joins_and_splits_without_decoding() {
		let parent = GitPath::from_utf8("src").unwrap();
		let child = GitPathComponent::from_bytes(b"\xff.rs".to_vec()).unwrap();
		let path = parent.join(&child);
		assert_eq!(path.as_bytes(), b"src/\xff.rs");
		assert_eq!(path.parent().unwrap(), parent);
		assert_eq!(path.file_name().unwrap(), child);
	}

	#[test]
	fn root_relationships_preserve_the_complete_path() {
		let root = GitPath::root();
		let path = GitPath::from_utf8("a/b").unwrap();
		assert!(path.is_below_with_ascii_case(&root, false));
		assert!(path.is_below_with_ascii_case(&root, true));
		assert!(!root.is_below_with_ascii_case(&root, false));
		assert_eq!(path.relative_to(&root), Some(path.clone()));
		assert_eq!(root.relative_to(&root), Some(root));
	}

	#[test]
	fn quotes_affixes_with_the_path() {
		let path = GitPath::from_utf8("line\nfile").unwrap();
		assert_eq!(path.quote_with_affixes(b"a/", b""), "\"a/line\\nfile\"");
		assert_eq!(path.quote_with_affixes(b"", b"/"), "\"line\\nfile/\"");
	}

	#[test]
	fn rendering_honours_the_non_ascii_quote_policy() {
		let utf8 = GitPath::from_utf8("café").unwrap();
		assert_eq!(
			utf8.render_with_affixes(b"", b"", true),
			b"\"caf\\303\\251\""
		);
		assert_eq!(utf8.render_with_affixes(b"", b"", false), "café".as_bytes());
		let raw = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		assert_eq!(raw.render_with_affixes(b"", b"", false), b"raw-\xff");
	}

	#[test]
	fn display_preserves_valid_utf8_and_quotes_invalid_bytes() {
		assert_eq!(GitPath::from_utf8("café").unwrap().to_string(), "café");
		assert_eq!(
			GitPath::from_utf8("line\nfile").unwrap().to_string(),
			"line\nfile"
		);
		assert_eq!(
			GitPath::from_bytes(b"raw-\xff".to_vec())
				.unwrap()
				.to_string(),
			"\"raw-\\377\""
		);
	}
}
