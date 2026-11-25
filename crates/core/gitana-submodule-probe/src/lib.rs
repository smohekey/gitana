//! Looks inside a work tree's submodules natively, as git's `status` does: the
//! [`SubmoduleProbe`](gitana_worktree::SubmoduleProbe) that gitana-worktree's status asks about each
//! gitlink, so a submodule's modified and untracked content, and its new commits, show as git shows
//! them.
//!
//! A submodule is found from its mount as git finds it: the mount's `.git`, a gitfile (followed
//! wherever it points) or a directory. Its ignore setting comes from the superproject's config,
//! `.gitmodules` and `diff.ignoreSubmodules`, read as git's status reads them. Everything here was
//! probed against git 2.55.

mod declarations;
mod module;
mod native_submodule_probe;

pub(crate) use self::declarations::Declarations;
pub(crate) use self::module::Module;
pub use self::native_submodule_probe::NativeSubmoduleProbe;

/// The folder at `path`, opened with the process's own authority.
pub(crate) fn open_dir(path: &std::path::Path) -> anyhow::Result<cap_std::fs::Dir> {
	use anyhow::Context as _;
	cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
		.with_context(|| format!("couldn't open {}", path.display()))
}

/// `path` (a gitlink's, `/`-separated) below `root`, as a native path.
pub(crate) fn native_path(
	root: &std::path::Path,
	path: &gitana_path::GitPath,
) -> std::path::PathBuf {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt as _;
		root.join(std::ffi::OsStr::from_bytes(path.as_bytes()))
	}
	#[cfg(not(unix))]
	{
		path
			.to_string()
			.split('/')
			.fold(root.to_path_buf(), |native, component| {
				native.join(component)
			})
	}
}
