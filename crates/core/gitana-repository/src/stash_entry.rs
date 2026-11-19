use gitana_object::{HashAlgorithm, ObjectId};

/// One stash, as `git stash list` shows it: `stash@{index}: message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashEntry<H: HashAlgorithm> {
	/// Its place in the list: 0 is the newest (`stash@{0}`).
	pub index: usize,
	/// The stash commit (`w`).
	pub commit: ObjectId<H>,
	/// The reflog message (`WIP on main: 1a2b3c4 subject`, or `On main: <message>`), read as UTF-8
	/// (any other bytes replaced).
	pub message: String,
	/// Who made it and when, the reflog's identity line (`Name <email> seconds ±hhmm`).
	pub committer: String,
}
