use crate::SubmoduleObjectId;

/// Git's leading status character for a tracked submodule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmoduleStatusState {
	Uninitialized,
	Current,
	Modified,
	Conflicted,
}

impl SubmoduleStatusState {
	pub fn sigil(self) -> char {
		match self {
			Self::Uninitialized => '-',
			Self::Current => ' ',
			Self::Modified => '+',
			Self::Conflicted => 'U',
		}
	}
}

/// One structured `submodule status` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleStatus {
	pub name: String,
	pub path: String,
	pub state: SubmoduleStatusState,
	pub oid: SubmoduleObjectId,
}
