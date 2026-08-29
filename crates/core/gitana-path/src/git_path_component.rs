use std::{borrow::Borrow, fmt, ops::Deref};

use super::GitPathError;

/// One exact byte-preserving component of a Git repository path.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitPathComponent(Vec<u8>);

impl GitPathComponent {
	/// Validates and owns raw Git tree-entry name bytes.
	pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, GitPathError> {
		let bytes = bytes.into();
		if bytes.contains(&0) {
			return Err(GitPathError::Nul);
		}
		if bytes.is_empty() || bytes.contains(&b'/') {
			return Err(GitPathError::NotComponent);
		}
		if bytes == b"." || bytes == b".." {
			return Err(GitPathError::Traversal);
		}
		Ok(Self(bytes))
	}

	/// Validates a UTF-8 component without changing its bytes.
	pub fn from_utf8(component: impl AsRef<str>) -> Result<Self, GitPathError> {
		Self::from_bytes(component.as_ref().as_bytes().to_vec())
	}

	/// Returns the exact component bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Consumes the component and returns its exact Git name bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}

	/// Returns the component as UTF-8 when possible.
	#[must_use]
	pub fn as_utf8(&self) -> Option<&str> {
		std::str::from_utf8(&self.0).ok()
	}

	/// Renders this component under Git's `core.quotePath` policy.
	#[must_use]
	pub fn render(&self, quote_non_ascii: bool) -> Vec<u8> {
		crate::render_bytes(&self.0, quote_non_ascii)
	}

	pub(crate) fn from_validated(bytes: Vec<u8>) -> Self {
		Self(bytes)
	}
}

impl AsRef<[u8]> for GitPathComponent {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Borrow<[u8]> for GitPathComponent {
	fn borrow(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Deref for GitPathComponent {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		self.as_bytes()
	}
}

impl PartialEq<str> for GitPathComponent {
	fn eq(&self, other: &str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<&str> for GitPathComponent {
	fn eq(&self, other: &&str) -> bool {
		self == *other
	}
}

impl fmt::Display for GitPathComponent {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_human(self.as_bytes(), formatter)
	}
}

impl fmt::Debug for GitPathComponent {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_tuple("GitPathComponent")
			.field(&self.0)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validates_one_raw_component() {
		assert_eq!(
			GitPathComponent::from_bytes(vec![0xff]).unwrap().as_bytes(),
			&[0xff]
		);
		assert_eq!(
			GitPathComponent::from_utf8("a/b").unwrap_err(),
			GitPathError::NotComponent
		);
		assert_eq!(
			GitPathComponent::from_utf8("..").unwrap_err(),
			GitPathError::Traversal
		);
	}

	#[test]
	fn display_preserves_valid_utf8_and_quotes_invalid_bytes() {
		assert_eq!(
			GitPathComponent::from_utf8("café").unwrap().to_string(),
			"café"
		);
		assert_eq!(
			GitPathComponent::from_bytes(b"raw-\xff".to_vec())
				.unwrap()
				.to_string(),
			"\"raw-\\377\""
		);
	}
}
