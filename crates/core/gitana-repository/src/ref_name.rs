use std::fmt;

/// A ref name git would accept (`git check-ref-format --allow-onelevel`): `refs/heads/topic`,
/// `refs/tags/v1`, or a one-level name such as `HEAD` that a symbolic ref may point at. Made only by
/// [`RefName::parse`]. A valid name has no `..`, no absolute or empty component and no `\`, so joined
/// to a repository's folder it stays inside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefName(String);

/// Why [`RefName::parse`] refused a name: it isn't one git would accept for a ref.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a valid ref name")]
pub struct InvalidRefName(pub String);

impl RefName {
	/// `name`, if git would accept it as a ref's name.
	pub fn parse(name: &str) -> Result<Self, InvalidRefName> {
		if is_valid_refname(name) {
			Ok(Self(name.to_owned()))
		} else {
			Err(InvalidRefName(name.to_owned()))
		}
	}

	/// The name as written: `refs/heads/topic`.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for RefName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

/// Whether `name` obeys git's ref-name rules, one-level names included: no component beginning with
/// `.` or ending with `.lock`; no `..`, `//`, leading or trailing `/`, or trailing `.`; no control
/// character, space or any of `~^:?*[\`; no `@{`; and not `@` alone. [`RefName::parse`] without the
/// allocation, for checks inside this crate.
pub(crate) fn is_valid_refname(name: &str) -> bool {
	if name.is_empty() || name == "@" {
		return false;
	}
	if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
		return false;
	}
	if name.contains("..") || name.contains("@{") || name.ends_with('.') {
		return false;
	}
	if name.bytes().any(|byte| {
		byte < 0x20
			|| byte == 0x7f
			|| matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
	}) {
		return false;
	}
	name
		.split('/')
		.all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn takes_what_git_takes_for_a_ref() {
		// As `git check-ref-format --allow-onelevel` takes them.
		for name in [
			"HEAD",
			"MERGE_HEAD",
			"refs/heads/topic",
			"refs/heads/feature/x",
			"refs/tags/v1.0",
			"refs/heads/-x",
			"refs/heads/HEAD",
			"refs/heads/@",
			"refs/heads/x@y",
			"refs/heads/a.b",
			"refs/heads/é",
		] {
			assert_eq!(RefName::parse(name).unwrap().as_str(), name, "{name}");
		}
		for name in [
			"",
			"@",
			"/refs/heads/a",
			"refs/heads/a/",
			"refs//heads",
			"refs/heads/../x",
			"refs/heads/a..b",
			"refs/heads/a.",
			"refs/heads/.a",
			"refs/heads/a.lock",
			"refs/heads/a.lock/b",
			"refs/heads/a@{1}",
			"refs/heads/a b",
			"refs/heads/a~1",
			"refs/heads/a^",
			"refs/heads/a:b",
			"refs/heads/a?",
			"refs/heads/a*",
			"refs/heads/a[",
			"refs/heads/a\\b",
			"refs/heads/a\tb",
			"refs/heads/a\u{7f}",
		] {
			assert_eq!(
				RefName::parse(name),
				Err(InvalidRefName(name.to_owned())),
				"{name:?}"
			);
		}
	}

	#[test]
	fn shows_its_name() {
		let name = RefName::parse("refs/tags/v1").unwrap();
		assert_eq!(name.to_string(), "refs/tags/v1");
	}
}
