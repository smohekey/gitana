use std::{borrow::Borrow, fmt, ops::Deref};

use super::{GitPathComponent, GitPathError};

/// Exact name bytes decoded from one Git tree entry.
///
/// This is a structural object-codec value, not a canonical repository path component. It therefore
/// preserves damaged or foreign names such as an empty name, `.`, `..`, or a slash-containing name so
/// plumbing can inspect the object. Convert to [`GitPathComponent`] before using a name as a path.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitTreeEntryName(Vec<u8>);

impl GitTreeEntryName {
	/// Own raw tree-entry name bytes. NUL is excluded because it terminates the name in a tree record.
	pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, GitPathError> {
		let bytes = bytes.into();
		if bytes.contains(&0) {
			return Err(GitPathError::Nul);
		}
		Ok(Self(bytes))
	}

	/// Preserve the exact bytes of a UTF-8 tree-entry name without canonical path validation.
	pub fn from_utf8(name: impl AsRef<str>) -> Result<Self, GitPathError> {
		Self::from_bytes(name.as_ref().as_bytes().to_vec())
	}

	/// Return the exact name bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Consume the name and return its exact bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}

	/// Render this tree-entry name under Git's `core.quotePath` policy.
	#[must_use]
	pub fn render(&self, quote_non_ascii: bool) -> Vec<u8> {
		crate::render_bytes(&self.0, quote_non_ascii)
	}
}

impl From<GitPathComponent> for GitTreeEntryName {
	fn from(component: GitPathComponent) -> Self {
		Self(component.into_bytes())
	}
}

impl TryFrom<&GitTreeEntryName> for GitPathComponent {
	type Error = GitPathError;

	fn try_from(name: &GitTreeEntryName) -> Result<Self, Self::Error> {
		Self::from_bytes(name.as_bytes().to_vec())
	}
}

impl TryFrom<GitTreeEntryName> for GitPathComponent {
	type Error = GitPathError;

	fn try_from(name: GitTreeEntryName) -> Result<Self, Self::Error> {
		Self::from_bytes(name.into_bytes())
	}
}

impl AsRef<[u8]> for GitTreeEntryName {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Borrow<[u8]> for GitTreeEntryName {
	fn borrow(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Deref for GitTreeEntryName {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		self.as_bytes()
	}
}

impl PartialEq<str> for GitTreeEntryName {
	fn eq(&self, other: &str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<&str> for GitTreeEntryName {
	fn eq(&self, other: &&str) -> bool {
		self == *other
	}
}

impl fmt::Display for GitTreeEntryName {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_human(self.as_bytes(), formatter)
	}
}

impl fmt::Debug for GitTreeEntryName {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_tuple("GitTreeEntryName")
			.field(&self.0)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn preserves_structural_names_until_canonical_conversion() {
		for bytes in [b"".as_slice(), b".", b"..", b"a/b", b"raw-\xff"] {
			let name = GitTreeEntryName::from_bytes(bytes.to_vec()).unwrap();
			assert_eq!(name.as_bytes(), bytes);
		}

		assert!(GitPathComponent::try_from(GitTreeEntryName::from_utf8(".").unwrap()).is_err());
		assert!(GitPathComponent::try_from(GitTreeEntryName::from_utf8("a/b").unwrap()).is_err());
		assert!(
			GitPathComponent::try_from(GitTreeEntryName::from_bytes(b"raw-\xff".to_vec()).unwrap())
				.is_ok()
		);
	}
}
