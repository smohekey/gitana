use std::{fmt, ops::Deref};

use super::GitPathError;

/// Exact command-boundary bytes for a Git pathspec.
///
/// Unlike [`super::GitPath`], a pathspec may contain magic, glob characters,
/// empty components, and traversal components that the pathspec parser resolves.
/// NUL is never valid because Git uses it as a record separator.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitPathspec(Vec<u8>);

impl GitPathspec {
	/// Validates and owns raw pathspec bytes.
	pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, GitPathError> {
		let bytes = bytes.into();
		if bytes.contains(&0) {
			return Err(GitPathError::Nul);
		}
		Ok(Self(bytes))
	}

	/// Preserves the exact bytes of a UTF-8 pathspec.
	pub fn from_utf8(pathspec: impl AsRef<str>) -> Result<Self, GitPathError> {
		Self::from_bytes(pathspec.as_ref().as_bytes().to_vec())
	}

	/// Returns the exact pathspec bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Returns the pathspec as UTF-8 when possible.
	#[must_use]
	pub fn as_utf8(&self) -> Option<&str> {
		std::str::from_utf8(&self.0).ok()
	}

	/// Consumes the value and returns its exact bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}

	/// Render this pathspec with reversible Git C-style quoting. Unlike human [`Display`](fmt::Display),
	/// non-ASCII bytes, quotes, and backslashes are escaped so distinct raw values remain distinct.
	#[must_use]
	pub fn quote(&self) -> String {
		crate::quote_bytes(&self.0)
	}
}

impl AsRef<[u8]> for GitPathspec {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Deref for GitPathspec {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		self.as_bytes()
	}
}

impl fmt::Display for GitPathspec {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_human(self.as_bytes(), formatter)
	}
}

impl fmt::Debug for GitPathspec {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_tuple("GitPathspec").field(&self.0).finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn preserves_magic_glob_and_non_utf8_bytes() {
		let bytes = b":(glob)dir/\xff*".to_vec();
		assert_eq!(
			GitPathspec::from_bytes(bytes.clone()).unwrap().as_bytes(),
			bytes
		);
		assert_eq!(
			GitPathspec::from_bytes(b"a\0b".to_vec()).unwrap_err(),
			GitPathError::Nul
		);
	}

	#[test]
	fn human_display_preserves_utf8_and_quotes_invalid_bytes() {
		assert_eq!(GitPathspec::from_utf8("café").unwrap().to_string(), "café");
		assert_eq!(
			GitPathspec::from_bytes(b"raw-\xff".to_vec())
				.unwrap()
				.to_string(),
			"\"raw-\\377\""
		);
	}

	#[test]
	fn reversible_quoting_distinguishes_a_human_display_collision() {
		let raw = GitPathspec::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = GitPathspec::from_utf8("\"raw-\\377\"").unwrap();
		assert_eq!(raw.to_string(), literal.to_string());
		assert_ne!(raw.quote(), literal.quote());
	}
}
