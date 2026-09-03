/// Path selection for a submodule operation.
#[derive(Clone, Debug, Default)]
pub struct SubmoduleQuery {
	pub pathspecs: Vec<String>,
}

impl SubmoduleQuery {
	pub fn all() -> Self {
		Self::default()
	}

	pub fn paths(pathspecs: Vec<String>) -> Self {
		Self { pathspecs }
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
}
