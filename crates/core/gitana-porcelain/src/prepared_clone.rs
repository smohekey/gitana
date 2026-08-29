use gitana_object::{HashAlgorithm, ObjectId};

/// Repository state produced by [`prepare_clone`](crate::prepare_clone) before a worktree checkout.
pub struct PreparedClone<H: HashAlgorithm> {
	/// The commit resolved through the prepared repository's `HEAD`, if the remote is not empty.
	pub head: Option<ObjectId<H>>,
}
