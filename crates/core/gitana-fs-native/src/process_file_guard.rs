use cap_std::fs::File;

/// Retained filesystem authority for a file consumed by a configured child process.
///
/// On Unix the guard owns a private snapshot that remains reopenable even when the child closes
/// inherited descriptors. On Windows the file was opened without write or delete sharing, so
/// retaining it keeps the child-facing pathname bound until the child opens it.
pub struct ProcessFileGuard {
	pub(crate) _file: File,
	#[cfg(all(unix, not(target_os = "fuchsia")))]
	pub(crate) _snapshot: tempfile::NamedTempFile,
	#[cfg(windows)]
	pub(crate) _pins: Vec<std::fs::File>,
}
