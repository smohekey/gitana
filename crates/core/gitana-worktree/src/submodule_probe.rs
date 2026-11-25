use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;

use crate::IgnoreSubmodules;

/// What a [`SubmoduleProbe`] found for one submodule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmoduleReport {
	/// The submodule's own ignore setting: `submodule.<name>.ignore` from the repository's config,
	/// else from `.gitmodules`, else `diff.ignoreSubmodules`.
	pub ignore: IgnoreSubmodules,
	/// The commit checked out in the submodule isn't the recorded one. The work tree finds this
	/// itself for the layout it can read (a gitfile into its own `modules/`); the probe covers the
	/// others, such as a `.git` directory in the mount.
	pub new_commits: bool,
	/// Tracked content inside the submodule is changed (staged or not), or a submodule inside it
	/// has new commits or modified content of its own.
	pub modified_content: bool,
	/// The submodule, or a submodule inside it, has untracked content (not ignored files).
	pub untracked_content: bool,
}

/// Looks inside the submodules of a [`WorkTree`](crate::WorkTree), which can't open a nested work
/// tree itself, so its status can report their content as git does. Attach one with
/// [`WorkTree::with_submodule_probe`](crate::WorkTree::with_submodule_probe).
///
/// The method returns a boxed future, rather than being an `async fn`, so the probe can be held as a
/// trait object.
pub trait SubmoduleProbe<H: HashAlgorithm>: Send + Sync {
	/// What's inside the submodule mounted at `path` (from the work tree's root), whose gitlink
	/// records `recorded`; or `None` when there's nothing to say, so the gitlink is compared by its
	/// checked-out commit alone.
	///
	/// `flag` is the command line's `--ignore-submodules`, when given. It outranks the submodule's
	/// own setting, which the report still gives.
	///
	/// It's asked for every gitlink status compares, mount deleted or replaced by a file included,
	/// since the submodule's setting can hide those too; with a `flag`, only when there's a mount.
	/// Like git, it shouldn't look at what the setting in effect hides: the content under `dirty`
	/// (new commits still count), anything under `all`. An error fails the status, naming the
	/// submodule.
	#[allow(clippy::type_complexity)]
	fn report<'a>(
		&'a self,
		path: &'a GitPath,
		recorded: ObjectId<H>,
		flag: Option<IgnoreSubmodules>,
	) -> Pin<
		Box<
			dyn Future<Output = Result<Option<SubmoduleReport>, Box<dyn Error + Send + Sync>>>
				+ Send
				+ 'a,
		>,
	>;
}
