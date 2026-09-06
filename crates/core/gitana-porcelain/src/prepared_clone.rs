use gitana_object::{HashAlgorithm, ObjectId};

/// Repository state produced by [`prepare_clone`](crate::prepare_clone) before a worktree checkout.
pub struct PreparedClone<H: HashAlgorithm> {
	/// The commit resolved through the prepared repository's `HEAD`, if the remote is not empty.
	pub head: Option<ObjectId<H>>,
	/// Object-graph roots requested while populating the repository.
	pub fetched_roots: Vec<ObjectId<H>>,
	/// Whether clone published the `origin/HEAD` convenience ref.
	pub remote_head_published: bool,
}
