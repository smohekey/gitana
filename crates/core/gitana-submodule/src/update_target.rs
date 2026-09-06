use crate::SubmoduleObjectId;

/// The commit-selection policy for one submodule update transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmoduleUpdateTarget {
	/// Use the commit recorded by the superproject gitlink.
	Gitlink(SubmoduleObjectId),
	/// Use the selected remote's advertised or locally recorded default branch.
	RemoteHead,
	/// Use the named branch from the selected remote.
	RemoteBranch(String),
}
