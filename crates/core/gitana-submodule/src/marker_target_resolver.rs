#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
use cap_std::fs::Dir;

#[cfg(not(target_arch = "wasm32"))]
use crate::SubmoduleError;

/// Frontend-owned authority for resolving native submodule mount-marker paths.
pub trait MarkerTargetResolver {
	/// Compare a marker target with the retained module repository capability without granting the
	/// core engine ambient filesystem authority.
	#[cfg(not(target_arch = "wasm32"))]
	async fn marker_target_matches(
		&self,
		mount_path: &Path,
		target: &str,
		expected_git_dir: Dir,
	) -> Result<bool, SubmoduleError>;
}
