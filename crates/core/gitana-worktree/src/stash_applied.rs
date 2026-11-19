use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;

/// What [`WorkTree::stash_apply`](crate::WorkTree::stash_apply) or
/// [`WorkTree::stash_pop`](crate::WorkTree::stash_pop) brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashApplied<H: HashAlgorithm> {
	/// The stash commit brought back.
	pub commit: ObjectId<H>,
	/// The paths left with conflict markers and stages; none when it applied cleanly (a pop then
	/// drops the stash, and keeps it otherwise).
	pub conflicts: Vec<GitPath>,
	/// Whether what was staged came back staged (`restore_index`, when there was anything to bring
	/// back and it applied cleanly).
	pub index_restored: bool,
	/// The index entries a file or folder the stash brought back took the place of, in path order:
	/// `p` for a stash that replaced the file `p` with `p/child`, `q/c` for one that replaced the
	/// folder `q/` with a file (git: "'<path>' appears as both a file and as a directory").
	pub replaced: Vec<GitPath>,
	/// Files of the user's that were in the way of the stash's new files outside the sparse
	/// checkout, each with the name it was moved to (`<path>.stash.<6 random letters or digits>`),
	/// in path order (git: "Untracked file in way of tracked file!  Renaming").
	pub moved_aside: Vec<(GitPath, GitPath)>,
}
