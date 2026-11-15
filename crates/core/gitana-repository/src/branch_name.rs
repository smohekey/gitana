use std::fmt;

use crate::is_valid_refname;

/// A branch name git would accept (`git check-ref-format --branch`): `refs/heads/<name>` is a valid
/// ref name, and the name is neither `HEAD` nor starts with `-`. Made only by [`BranchName::parse`],
/// so an unchecked name never reaches the operations that take one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BranchName(String);

/// Why [`BranchName::parse`] refused a name: it isn't one git would accept for a branch.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a valid branch name")]
pub struct InvalidBranchName(pub String);

impl BranchName {
	/// `name`, if git would accept it as a branch's name.
	pub fn parse(name: &str) -> Result<Self, InvalidBranchName> {
		let valid =
			!name.starts_with('-') && name != "HEAD" && is_valid_refname(&format!("refs/heads/{name}"));
		if valid {
			Ok(Self(name.to_owned()))
		} else {
			Err(InvalidBranchName(name.to_owned()))
		}
	}

	/// The name as written: `topic`.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// Its ref: `refs/heads/topic`.
	pub fn full(&self) -> String {
		format!("refs/heads/{}", self.0)
	}
}

impl fmt::Display for BranchName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn takes_what_git_takes_for_a_branch() {
		// As stock git takes them (`git branch` makes `@` and `x@y`).
		for name in ["topic", "feature/x", "a-b_c.d", "v1.0", "x@y", "@"] {
			assert_eq!(BranchName::parse(name).unwrap().as_str(), name, "{name}");
		}
		for name in [
			"", "-x", "HEAD", "a..b", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "/a", "a/",
			"a//b", ".a", "a/.b", "a.lock", "a/b.lock", "a.", "a@{1}", "a\u{7f}",
		] {
			assert_eq!(
				BranchName::parse(name),
				Err(InvalidBranchName(name.to_owned())),
				"{name:?}"
			);
		}
	}

	#[test]
	fn names_its_ref() {
		let name = BranchName::parse("feature/x").unwrap();
		assert_eq!(name.full(), "refs/heads/feature/x");
		assert_eq!(name.to_string(), "feature/x");
	}
}
