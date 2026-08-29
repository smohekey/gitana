/// Resolve a leading `./` or `../` submodule URL against the superproject remote URL.
pub fn resolve_relative_url(base: &str, url: &str) -> Result<String, RelativeUrlError> {
	if !(url.starts_with("./") || url.starts_with("../")) {
		return Ok(url.to_owned());
	}
	if let Some(windows) = WindowsLocalBase::parse(base) {
		return windows.resolve(url);
	}
	resolve_forward_slashes(base, url)
}

fn resolve_forward_slashes(base: &str, url: &str) -> Result<String, RelativeUrlError> {
	let trimmed = base.trim_end_matches('/');
	let anchored = trimmed.starts_with('/') || trimmed.contains(':');
	let mut resolved = trimmed.to_owned();
	let mut colon_separator = false;
	let mut rest = url;
	loop {
		if let Some(next) = rest.strip_prefix("../") {
			rest = next;
			if let Some(slash) = resolved.rfind('/') {
				resolved.truncate(slash);
				colon_separator = false;
			} else if let Some(colon) = resolved
				.rfind(':')
				.filter(|colon| *colon + 1 < resolved.len())
			{
				resolved.truncate(colon + 1);
				colon_separator = true;
			} else if resolved == "." {
				return Err(RelativeUrlError);
			} else {
				// Git uses `.` as the sentinel once a base with no remaining separator has been
				// stripped. Rendering later removes the leading `./`; another parent is rejected.
				resolved.replace_range(.., ".");
			}
		} else if let Some(next) = rest.strip_prefix("./") {
			rest = next;
		} else {
			break;
		}
	}
	if colon_separator {
		if resolved == "." {
			resolved.push(':');
		}
	} else if !resolved.is_empty() || anchored {
		resolved.push('/');
	}
	resolved.push_str(rest);
	let resolved = resolved.strip_prefix("./").unwrap_or(&resolved).to_owned();
	if resolved.is_empty() {
		Err(RelativeUrlError)
	} else {
		Ok(resolved)
	}
}

/// A Windows-native local path rendered independently of the host platform. Relative submodule URL
/// resolution is string based, so tests and callers must not depend on the build host's `Path`
/// parser to distinguish a drive colon from an scp separator.
enum WindowsLocalBase<'a> {
	Ordinary(&'a str),
	VerbatimDrive { prefix: &'a str, path: &'a str },
	VerbatimUnc(&'a str),
}

impl<'a> WindowsLocalBase<'a> {
	fn parse(base: &'a str) -> Option<Self> {
		if let Some(path) = base.strip_prefix(r"\\?\UNC\") {
			return Some(Self::VerbatimUnc(path));
		}
		if let Some(path) = base.strip_prefix(r"\\?\") {
			return windows_drive_absolute(path).then_some(Self::VerbatimDrive {
				prefix: r"\\?\",
				path,
			});
		}
		if let Some(path) = base.strip_prefix(r"\\.\") {
			return windows_drive_absolute(path).then_some(Self::VerbatimDrive {
				prefix: r"\\.\",
				path,
			});
		}
		(windows_drive_absolute(base) || base.starts_with(r"\\") || base.starts_with("//"))
			.then_some(Self::Ordinary(base))
	}

	fn resolve(self, url: &str) -> Result<String, RelativeUrlError> {
		match self {
			Self::Ordinary(base) => {
				let normalized = base.replace('\\', "/");
				resolve_forward_slashes(&normalized, url)
			}
			Self::VerbatimDrive { prefix, path } => {
				let normalized = path.replace('\\', "/");
				let resolved = resolve_forward_slashes(&normalized, url)?;
				Ok(format!("{prefix}{}", resolved.replace('/', r"\")))
			}
			Self::VerbatimUnc(path) => {
				let normalized = format!("//{}", path.replace('\\', "/"));
				let resolved = resolve_forward_slashes(&normalized, url)?;
				let resolved = resolved.strip_prefix("//").ok_or(RelativeUrlError)?;
				Ok(format!(r"\\?\UNC\{}", resolved.replace('/', r"\")))
			}
		}
	}
}

fn windows_drive_absolute(path: &str) -> bool {
	let bytes = path.as_bytes();
	bytes.len() >= 3
		&& bytes[0].is_ascii_alphabetic()
		&& bytes[1] == b':'
		&& matches!(bytes[2], b'/' | b'\\')
}

/// A relative submodule URL could not be resolved without escaping its base.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("relative URL escapes its base")]
pub struct RelativeUrlError;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn resolves_url_scp_and_local_bases() {
		for (base, relative, expected) in [
			(
				"https://example.com/team/super.git",
				"../sub",
				"https://example.com/team/sub",
			),
			(
				"git@example.com:team/super",
				"../sub",
				"git@example.com:team/sub",
			),
			("/srv/team/super", "../sub", "/srv/team/sub"),
		] {
			assert_eq!(resolve_relative_url(base, relative).unwrap(), expected);
		}
	}

	#[test]
	fn rejects_unanchored_base_escape_and_empty_result() {
		assert_eq!(resolve_relative_url("foo", "../bar").unwrap(), "bar");
		assert_eq!(
			resolve_relative_url("foo", "../../bar"),
			Err(RelativeUrlError)
		);
		assert_eq!(resolve_relative_url("foo", "../"), Err(RelativeUrlError));
	}

	#[test]
	fn local_root_traversal_matches_git_component_stripping() {
		assert_eq!(
			resolve_relative_url("/srv/team/super", "../../../sub").unwrap(),
			"/sub"
		);
		assert_eq!(
			resolve_relative_url("/srv/team/super", "../../../../sub").unwrap(),
			"sub"
		);
		assert_eq!(
			resolve_relative_url("/srv/team/super", "../../../../../sub"),
			Err(RelativeUrlError)
		);
		assert_eq!(
			resolve_relative_url("git@example.com:team/super", "../../../sub").unwrap(),
			".:sub"
		);
	}

	#[test]
	fn resolves_windows_local_bases_without_treating_the_drive_as_scp() {
		for (base, expected) in [
			(r"C:\team\super", "C:/team/sub"),
			("C:/team/super", "C:/team/sub"),
			(r"\\server\share\team\super", "//server/share/team/sub"),
			(r"\\?\C:\team\super", r"\\?\C:\team\sub"),
			(
				r"\\?\UNC\server\share\team\super",
				r"\\?\UNC\server\share\team\sub",
			),
		] {
			assert_eq!(resolve_relative_url(base, "../sub").unwrap(), expected);
		}
	}
}
