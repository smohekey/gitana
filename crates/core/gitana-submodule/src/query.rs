use gitana_worktree::PathspecSet;

use crate::SubmoduleError;

/// Path selection for a submodule operation.
#[derive(Clone, Debug, Default)]
pub struct SubmoduleQuery {
	pub pathspecs: Vec<String>,
	/// Whether positive pathspecs that match no tracked gitlink are accepted.
	///
	/// Git's recursive-clone pathspecs are activation selectors, so a selector may legitimately match
	/// no module in the cloned commit. Ordinary submodule commands retain their strict pathspec errors.
	pub allow_unmatched: bool,
}

impl SubmoduleQuery {
	pub fn all() -> Self {
		Self::default()
	}

	pub fn paths(pathspecs: Vec<String>) -> Self {
		Self {
			pathspecs,
			allow_unmatched: false,
		}
	}

	/// Pathspec selection for recursive clone, where an unmatched activation selector is a no-op.
	pub fn paths_allow_unmatched(pathspecs: Vec<String>) -> Self {
		Self {
			pathspecs,
			allow_unmatched: true,
		}
	}

	/// Validate the pathspec syntax relative to `prefix` without requiring a candidate match.
	///
	/// Recursive clone uses this after persisting its activation selectors, then selects candidates
	/// from the complete effective activation configuration rather than this request alone.
	pub fn validate_pathspecs(&self, prefix: &str) -> Result<(), SubmoduleError> {
		let pathspecs = self
			.pathspecs
			.iter()
			.map(String::as_str)
			.collect::<Vec<_>>();
		PathspecSet::parse(&pathspecs, prefix)?;
		Ok(())
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn top_literal(path: &str) -> Self {
		Self::paths(vec![format!(":(top,literal){path}")])
	}
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use super::SubmoduleQuery;
	use gitana_worktree::PathspecSet;

	#[test]
	fn recovery_paths_are_literal_and_top_relative() {
		for (path, glob_match) in [
			("a*", "abc"),
			("a?", "ab"),
			("a[bc]", "ab"),
			(":(top)owned", "owned"),
		] {
			let query = SubmoduleQuery::top_literal(path);
			let pathspecs = query
				.pathspecs
				.iter()
				.map(String::as_str)
				.collect::<Vec<_>>();
			let set = PathspecSet::parse(&pathspecs, "nested").unwrap();
			assert!(
				set.matches(path),
				"recorded path {path:?} must match itself"
			);
			assert!(
				!set.matches(glob_match),
				"recorded path {path:?} must not select {glob_match:?}"
			);
		}
	}

	#[test]
	fn only_clone_queries_allow_unmatched_pathspecs() {
		assert!(!SubmoduleQuery::paths(vec!["missing".to_owned()]).allow_unmatched);
		assert!(SubmoduleQuery::paths_allow_unmatched(vec!["missing".to_owned()]).allow_unmatched);
	}

	#[test]
	fn clone_queries_validate_syntax_without_requiring_a_match() {
		SubmoduleQuery::paths_allow_unmatched(vec!["missing".to_owned()])
			.validate_pathspecs("")
			.unwrap();
		for pathspec in ["", "../outside", ":(unknown)path"] {
			assert!(
				SubmoduleQuery::paths_allow_unmatched(vec![pathspec.to_owned()])
					.validate_pathspecs("")
					.is_err(),
				"{pathspec:?} must be rejected"
			);
		}
	}
}
