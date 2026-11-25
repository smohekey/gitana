use gitana_config::GitConfig;

use crate::WorktreeError;

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

	/// The `diff.ignoreSubmodules` setting in `config`, refused as git refuses it. git checks every
	/// entry as it reads the config, so a bad or valueless one fails even with a valid one after it.
	/// An empty value is a bad one, not a missing one. The last entry wins (probed vs git 2.55).
	pub fn from_diff_config(config: &GitConfig) -> Result<Option<Self>, WorktreeError> {
		let mut setting = None;
		for value in config.get_all_raw("diff", None, "ignoresubmodules") {
			let value = value.ok_or(WorktreeError::MissingIgnoreSubmodulesValue)?;
			setting = Some(
				Self::parse(value).ok_or_else(|| WorktreeError::BadIgnoreSubmodules {
					value: value.to_owned(),
				})?,
			);
		}
		Ok(setting)
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
