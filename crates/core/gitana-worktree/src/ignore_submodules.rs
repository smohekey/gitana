/// Which of a submodule's changes status leaves out: git's `--ignore-submodules=<when>`,
/// `submodule.<name>.ignore` and `diff.ignoreSubmodules`.
///
/// Set in config (`.gitmodules`, `submodule.<name>.ignore`, `diff.ignoreSubmodules`), it covers only
/// the work-tree column. Given on the command line ([`WorkTree::with_ignore_submodules`]), `All`
/// also hides staged gitlink changes (probed vs git 2.55).
///
/// [`WorkTree::with_ignore_submodules`]: crate::WorkTree::with_ignore_submodules
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IgnoreSubmodules {
	/// Nothing: new commits, modified content and untracked content all count (git's default).
	#[default]
	None,
	/// Untracked content inside the submodule.
	Untracked,
	/// The submodule's own work tree: modified and untracked content. New commits still count.
	Dirty,
	/// All of it, new commits included, and a deleted or replaced mount (` D`, ` T`).
	All,
}

impl IgnoreSubmodules {
	/// The setting git spells `value`: exactly `none`, `untracked`, `dirty` or `all` (git matches
	/// them case-sensitively), else `None`.
	pub fn parse(value: &str) -> Option<Self> {
		match value {
			"none" => Some(Self::None),
			"untracked" => Some(Self::Untracked),
			"dirty" => Some(Self::Dirty),
			"all" => Some(Self::All),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::IgnoreSubmodules;

	#[test]
	fn parses_git_s_spellings_only() {
		assert_eq!(
			IgnoreSubmodules::parse("none"),
			Some(IgnoreSubmodules::None)
		);
		assert_eq!(
			IgnoreSubmodules::parse("untracked"),
			Some(IgnoreSubmodules::Untracked)
		);
		assert_eq!(
			IgnoreSubmodules::parse("dirty"),
			Some(IgnoreSubmodules::Dirty)
		);
		assert_eq!(IgnoreSubmodules::parse("all"), Some(IgnoreSubmodules::All));
		for value in ["ALL", "All", "Dirty", "bogus", ""] {
			assert_eq!(IgnoreSubmodules::parse(value), None, "{value:?}");
		}
	}
}
