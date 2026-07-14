//! Small observed-fact types shared by inspection and enumeration.

/// The kind of a worktree's `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadKind {
	/// `HEAD` names a branch (`ref: refs/heads/...`) that resolves to a commit.
	Symbolic,
	/// `HEAD` holds a raw commit id (detached).
	Detached,
	/// `HEAD` names a branch whose ref does not exist yet (a fresh checkout / empty repo).
	Unborn,
}

/// Whether a worktree registration is locked (git's `<admin>/locked` marker), and its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
	/// Not locked.
	Unlocked,
	/// Locked. `reason` is the text git recorded — `Some("")` when locked without a reason, `Some(text)`
	/// otherwise, `None` when the reason could not be read.
	Locked {
		/// The lock reason, if any.
		reason: Option<String>,
	},
}
