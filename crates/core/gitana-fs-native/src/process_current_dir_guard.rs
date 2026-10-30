use cap_std::fs::Dir;

/// Retained filesystem authority for a configured child process's working directory.
///
/// On Windows the retained directory handle denies delete sharing, so while it is open neither the
/// directory nor any directory above it can be renamed or replaced. The child opens its own
/// working-directory handle only during its startup, after `CreateProcess` returns, so keep this
/// guard, or another handle to the same retained directory, until the child has exited.
///
/// This guard owns filesystem handles, not the child process. Dropping it does not terminate the
/// child or wait for exit. Callers using best-effort cancellation can therefore release protection
/// before termination completes, even if they request termination when dropping the child.
///
/// It cannot stop a principal holding attribute-write access to an *empty* working directory from
/// converting it into a junction. A conversion during child startup redirects the child; afterwards
/// it redirects lookups the child makes by path, while relative opens through its own handle fail.
///
/// On Unix the retained directory is also captured by the command's pre-exec hook, while this guard
/// keeps the caller's authority explicit. Unix callers must not rely on this guard to stabilize `..`:
/// moving the retained directory to a different parent changes parent-relative resolution, so such
/// resource paths must be rejected or independently pinned before configuring the process.
pub struct ProcessCurrentDirGuard {
	pub(crate) _directory: Dir,
}
