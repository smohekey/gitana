use std::{cmp::Ordering, fmt, ops::Deref};

use super::{GitPath, GitPathComponent, GitPathError, GitTreeEntryName};

/// An exact recursively flattened path decoded from Git tree entries.
///
/// Unlike [`GitPath`], this type preserves noncanonical entry names so read-only plumbing can inspect
/// damaged or foreign trees. Component boundaries are retained separately from the flattened `/`-joined
/// bytes: a single tree name `a/b` is therefore distinct from canonical nested entries `a` then `b`, and
/// cannot become a filesystem path without validating the original components.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct GitTreePath {
	bytes: Vec<u8>,
	component_ends: Vec<usize>,
}

impl GitTreePath {
	/// Build a one-component tree path from an exact entry name.
	#[must_use]
	pub fn from_entry_name(name: &GitTreeEntryName) -> Self {
		Self {
			bytes: name.as_bytes().to_vec(),
			component_ends: vec![name.as_bytes().len()],
		}
	}

	/// Append an exact child entry name while retaining its structural boundary.
	#[must_use]
	pub fn join(&self, name: &GitTreeEntryName) -> Self {
		let mut bytes = self.bytes.clone();
		let mut component_ends = self.component_ends.clone();
		if !component_ends.is_empty() {
			bytes.push(b'/');
		}
		bytes.extend_from_slice(name.as_bytes());
		component_ends.push(bytes.len());
		Self {
			bytes,
			component_ends,
		}
	}

	/// Return the exact `/`-joined bytes Git presents for this recursive path.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.bytes
	}

	/// Consume the path and return its exact flattened bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.bytes
	}

	/// Return the flattened path as UTF-8 when its bytes are valid UTF-8.
	#[must_use]
	pub fn as_utf8(&self) -> Option<&str> {
		std::str::from_utf8(&self.bytes).ok()
	}

	/// Iterate over the original tree-entry components without decoding them.
	pub fn components(&self) -> impl Iterator<Item = &[u8]> {
		self.component_ends.iter().enumerate().map(|(index, end)| {
			let start = if index == 0 {
				0
			} else {
				self.component_ends[index - 1] + 1
			};
			&self.bytes[start..*end]
		})
	}

	/// Render the flattened path with raw display affixes under Git's `core.quotePath` policy.
	#[must_use]
	pub fn render_with_affixes(
		&self,
		prefix: &[u8],
		suffix: &[u8],
		quote_non_ascii: bool,
	) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(prefix.len() + self.bytes.len() + suffix.len());
		bytes.extend_from_slice(prefix);
		bytes.extend_from_slice(&self.bytes);
		bytes.extend_from_slice(suffix);
		crate::render_bytes(&bytes, quote_non_ascii)
	}
}

impl From<GitPath> for GitTreePath {
	fn from(path: GitPath) -> Self {
		let bytes = path.into_bytes();
		let mut component_ends = Vec::new();
		if !bytes.is_empty() {
			component_ends.extend(
				bytes
					.iter()
					.enumerate()
					.filter_map(|(index, byte)| (*byte == b'/').then_some(index)),
			);
			component_ends.push(bytes.len());
		}
		Self {
			bytes,
			component_ends,
		}
	}
}

impl TryFrom<&GitTreePath> for GitPath {
	type Error = GitPathError;

	fn try_from(path: &GitTreePath) -> Result<Self, Self::Error> {
		let mut canonical = Self::root();
		for component in path.components() {
			canonical = canonical.join(&GitPathComponent::from_bytes(component.to_vec())?);
		}
		Ok(canonical)
	}
}

impl TryFrom<GitTreePath> for GitPath {
	type Error = GitPathError;

	fn try_from(path: GitTreePath) -> Result<Self, Self::Error> {
		Self::try_from(&path)
	}
}

impl AsRef<[u8]> for GitTreePath {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Deref for GitTreePath {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		self.as_bytes()
	}
}

impl Ord for GitTreePath {
	fn cmp(&self, other: &Self) -> Ordering {
		self
			.bytes
			.cmp(&other.bytes)
			.then_with(|| self.component_ends.cmp(&other.component_ends))
	}
}

impl PartialOrd for GitTreePath {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl PartialEq<str> for GitTreePath {
	fn eq(&self, other: &str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl PartialEq<&str> for GitTreePath {
	fn eq(&self, other: &&str) -> bool {
		self == *other
	}
}

impl fmt::Display for GitTreePath {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		crate::write_human(self.as_bytes(), formatter)
	}
}

impl fmt::Debug for GitTreePath {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("GitTreePath")
			.field("bytes", &self.bytes)
			.field("component_ends", &self.component_ends)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn name(bytes: &[u8]) -> GitTreeEntryName {
		GitTreeEntryName::from_bytes(bytes.to_vec()).unwrap()
	}

	#[test]
	fn retains_structural_boundaries_while_rendering_git_paths() {
		let embedded_slash = GitTreePath::from_entry_name(&name(b"a/b"));
		let nested = GitTreePath::from_entry_name(&name(b"a")).join(&name(b"b"));

		assert_eq!(embedded_slash.as_bytes(), b"a/b");
		assert_eq!(nested.as_bytes(), b"a/b");
		assert_ne!(embedded_slash, nested);
		assert_ne!(embedded_slash.cmp(&nested), Ordering::Equal);
		assert!(GitPath::try_from(&embedded_slash).is_err());
		assert_eq!(
			GitPath::try_from(&nested).unwrap(),
			GitPath::from_utf8("a/b").unwrap()
		);
	}

	#[test]
	fn rejects_noncanonical_components_only_at_the_filesystem_boundary() {
		for bytes in [b"".as_slice(), b".", b"..", b"a/b"] {
			let path = GitTreePath::from_entry_name(&name(bytes));
			assert_eq!(path.as_bytes(), bytes);
			assert!(GitPath::try_from(path).is_err());
		}

		let raw = GitTreePath::from_entry_name(&name(b"raw-\xff"));
		assert!(GitPath::try_from(&raw).is_ok());
		assert_eq!(
			raw.render_with_affixes(b"a/", b"", true),
			b"\"a/raw-\\377\""
		);
	}

	#[test]
	fn canonical_paths_round_trip_with_their_component_boundaries() {
		for path in ["", "a", "a/b"] {
			let canonical = GitPath::from_utf8(path).unwrap();
			let raw = GitTreePath::from(canonical.clone());
			assert_eq!(raw.as_bytes(), canonical.as_bytes());
			assert_eq!(GitPath::try_from(raw).unwrap(), canonical);
		}
	}
}
