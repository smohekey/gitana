use std::fmt;

/// A multi-step operation left in progress in a worktree, which commits, switches and resets must
/// not run across unawares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
	Merge,
	CherryPick,
	Revert,
	Rebase,
}

impl Operation {
	/// Its name, as git's messages say it: `merge`, `cherry-pick`, `revert`, `rebase`.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Merge => "merge",
			Self::CherryPick => "cherry-pick",
			Self::Revert => "revert",
			Self::Rebase => "rebase",
		}
	}
}

impl fmt::Display for Operation {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}
