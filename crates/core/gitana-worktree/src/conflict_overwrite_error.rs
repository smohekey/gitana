use std::fmt;

use gitana_path::GitPath;

/// A merge-like operation would overwrite unstaged changes at one or more conflicted paths.
///
/// Paths remain typed until a frontend chooses human or reversible rendering. This preserves the
/// historical list-shaped human message without exposing [`GitPath`]'s debug wrapper and lets MCP
/// distinguish arbitrary byte paths from UTF-8 names that resemble their quoted spelling.
#[derive(Debug)]
pub struct ConflictOverwriteError {
	paths: Vec<GitPath>,
}

impl ConflictOverwriteError {
	/// Create a refusal for the conflicted paths whose local contents would be overwritten.
	pub fn new(paths: Vec<GitPath>) -> Self {
		Self { paths }
	}

	/// Render the refusal with the frontend's pathname policy.
	pub fn render_with_paths(&self, render_path: impl Fn(&GitPath) -> String) -> String {
		let paths = self.paths.iter().map(render_path).collect::<Vec<_>>();
		format!(
			"your local changes to {paths:?} would be overwritten by the merge; commit or stash them first"
		)
	}
}

impl fmt::Display for ConflictOverwriteError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.render_with_paths(ToString::to_string))
	}
}

impl std::error::Error for ConflictOverwriteError {}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn overwrite_error_preserves_human_shape_and_reversible_identity() {
		let ordinary = ConflictOverwriteError::new(vec![GitPath::from_utf8("f.txt").unwrap()]);
		assert_eq!(
			ordinary.to_string(),
			"your local changes to [\"f.txt\"] would be overwritten by the merge; commit or stash them first"
		);

		let raw = ConflictOverwriteError::new(vec![GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap()]);
		let literal = ConflictOverwriteError::new(vec![GitPath::from_utf8("\"raw-\\377\"").unwrap()]);
		assert_eq!(raw.to_string(), literal.to_string());
		assert_ne!(
			raw.render_with_paths(|path| path.quote_with_affixes(b"", b"")),
			literal.render_with_paths(|path| path.quote_with_affixes(b"", b""))
		);
	}
}
