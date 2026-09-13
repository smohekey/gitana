use cap_std::fs::Dir;

/// Retained filesystem authority needed until a configured child process has spawned.
///
/// On Windows these handles prevent the visible working-directory chain from being renamed after
/// validation but before `CreateProcess` consumes its path. On Unix the retained directory is also
/// captured by the command's pre-exec hook, while this guard keeps the caller's authority explicit.
/// Unix callers must not rely on this guard to stabilize `..`: moving the retained directory to a
/// different parent changes parent-relative resolution, so such resource paths must be rejected or
/// independently pinned before configuring the process.
pub struct ProcessCurrentDirGuard {
	pub(crate) _directory: Dir,
	#[cfg(windows)]
	pub(crate) _pins: Vec<std::fs::File>,
}
