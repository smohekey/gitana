use std::path::{Path, PathBuf};

/// Remove `.` and collapse `..` components without consulting the filesystem.
pub fn lexical_normalize(path: &Path) -> PathBuf {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			std::path::Component::CurDir => {}
			std::path::Component::ParentDir => {
				normalized.pop();
			}
			other => normalized.push(other.as_os_str()),
		}
	}
	normalized
}

/// Compare normalized native paths using the platform's component semantics.
pub fn paths_equivalent(left: &Path, right: &Path) -> bool {
	strip_path_prefix(left, right).is_some_and(|suffix| suffix.as_os_str().is_empty())
		&& strip_path_prefix(right, left).is_some_and(|suffix| suffix.as_os_str().is_empty())
}

/// Strip a normalized native path prefix using the platform's component semantics.
pub fn strip_path_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
	#[cfg(not(windows))]
	{
		path.strip_prefix(prefix).ok().map(Path::to_path_buf)
	}

	#[cfg(windows)]
	{
		strip_path_prefix_by(path, prefix, crate::windows::os_str_eq_ignore_case)
	}
}

#[cfg(any(windows, test))]
fn strip_path_prefix_by(
	path: &Path,
	prefix: &Path,
	mut equivalent: impl FnMut(&std::ffi::OsStr, &std::ffi::OsStr) -> bool,
) -> Option<PathBuf> {
	let mut path = path.components();
	for expected in prefix.components() {
		let actual = path.next()?;
		if !equivalent(actual.as_os_str(), expected.as_os_str()) {
			return None;
		}
	}
	let mut suffix = PathBuf::new();
	for component in path {
		suffix.push(component.as_os_str());
	}
	Some(suffix)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{lexical_normalize, strip_path_prefix_by};

	#[test]
	fn lexical_normalization_collapses_dot_components_without_filesystem_access() {
		assert_eq!(
			lexical_normalize(Path::new("/repo/.git/modules/./one/../../two")),
			Path::new("/repo/.git/two")
		);
	}

	#[test]
	fn windows_style_component_comparison_preserves_the_suffix() {
		let suffix = strip_path_prefix_by(
			Path::new("/Repo/.GIT/Modules/One"),
			Path::new("/repo/.git/modules"),
			|left, right| {
				left
					.to_string_lossy()
					.eq_ignore_ascii_case(&right.to_string_lossy())
			},
		)
		.unwrap();
		assert_eq!(suffix, Path::new("One"));
	}
}
