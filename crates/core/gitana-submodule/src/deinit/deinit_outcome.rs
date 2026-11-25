use std::path::PathBuf;

/// What deinit changed for one selected submodule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeinitOutcome {
	pub name: String,
	pub path: String,
	pub cleared: bool,
	pub unregistered: bool,
	/// The `.git` directories of submodules inside it that were moved into their parents' git
	/// directories before it was checked, as git's deinit absorbs them; depth first.
	pub absorbed: Vec<AbsorbedGitDir>,
}

/// A nested submodule's `.git` directory, moved into its parent's `modules/<name>` (git prints
/// "Migrating git directory of '<path>' from '<from>' to '<to>'").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbsorbedGitDir {
	/// The nested submodule's path from the superproject's work tree.
	pub path: String,
	/// Where its `.git` directory was.
	pub from: PathBuf,
	/// Where it is now.
	pub to: PathBuf,
}
