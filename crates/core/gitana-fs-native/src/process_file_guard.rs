use cap_std::fs::File;

/// Retained filesystem authority for a file consumed by a configured child process.
///
/// On Unix the guard owns a private snapshot that remains reopenable even when the child closes
/// inherited descriptors. On Windows the file was opened without write or delete sharing, so while
/// the guard lives neither the file nor any directory above it can be renamed or replaced,
/// protecting its resolved final path until the child opens it. Aliases in a compatibility fallback
/// spelling are not stabilized; see [`crate::configure_process_file`].
///
/// This guard does not own or wait for the child process. Dropping it releases its handles and runs
/// any private-snapshot cleanup without waiting for child exit. A caller that only requests
/// termination on cancellation does not establish that the child has exited before cleanup runs.
pub struct ProcessFileGuard {
	pub(crate) _file: File,
	#[cfg(all(unix, not(target_os = "fuchsia")))]
	pub(crate) _snapshot: tempfile::NamedTempFile,
}
