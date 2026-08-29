use std::fmt;

use gitana_path::{GitPath, GitPathspec};

/// A cone-mode sparse-checkout argument that Git rejects.
///
/// The offending path remains typed until the frontend chooses human or reversible rendering.
#[derive(Debug)]
pub(crate) enum SparseCheckoutError {
	PatternCharacter(GitPathspec),
	LeadingSlash(GitPathspec),
	OutsideRepository(GitPathspec),
	TrackedFile(GitPath),
}

impl SparseCheckoutError {
	pub(crate) fn render_with_paths(
		&self,
		render_path: impl Fn(&GitPath) -> String,
		render_pathspec: impl Fn(&GitPathspec) -> String,
	) -> String {
		match self {
			Self::PatternCharacter(pathspec) => format!(
				"'{}' contains a pattern character; cone directories must be literal paths",
				render_pathspec(pathspec)
			),
			Self::LeadingSlash(pathspec) => format!(
				"'{}': specify directories rather than patterns (no leading slash) in cone mode",
				render_pathspec(pathspec)
			),
			Self::OutsideRepository(pathspec) => {
				format!("'{}' is outside the repository", render_pathspec(pathspec))
			}
			Self::TrackedFile(path) => {
				format!("'{}' is a tracked file, not a directory", render_path(path))
			}
		}
	}
}

impl fmt::Display for SparseCheckoutError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.render_with_paths(ToString::to_string, ToString::to_string))
	}
}

impl std::error::Error for SparseCheckoutError {}

#[cfg(test)]
mod tests {
	use super::*;

	fn reversible(error: &SparseCheckoutError) -> String {
		error.render_with_paths(|path| path.quote_with_affixes(b"", b""), GitPathspec::quote)
	}

	#[test]
	fn frontend_rendering_preserves_rejected_path_identity() {
		let raw_spec = GitPathspec::from_bytes(b"raw-\xff*".to_vec()).unwrap();
		let literal_spec = GitPathspec::from_utf8("\"raw-\\377*\"").unwrap();
		let raw_path = GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal_path = GitPath::from_utf8("\"raw-\\377\"").unwrap();
		let pairs = [
			(
				SparseCheckoutError::PatternCharacter(raw_spec),
				SparseCheckoutError::PatternCharacter(literal_spec),
			),
			(
				SparseCheckoutError::TrackedFile(raw_path),
				SparseCheckoutError::TrackedFile(literal_path),
			),
		];

		for (raw, literal) in pairs {
			assert_eq!(raw.to_string(), literal.to_string());
			assert_ne!(reversible(&raw), reversible(&literal));
		}
	}
}
