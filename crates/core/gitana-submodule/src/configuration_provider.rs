use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
use cap_std::fs::Dir;
use gitana_config::GitConfig;
#[cfg(not(target_arch = "wasm32"))]
use gitana_object::HashKind;

use crate::SubmoduleError;

/// One repository-local configuration update planned by submodule initialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitConfigUpdate {
	pub name: String,
	pub activate: bool,
	pub url_if_absent: Option<String>,
	pub update_if_absent: Option<String>,
}

/// Result of atomically applying a batch of initialization updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitConfigResult {
	/// Names whose missing repository-local URL was installed by this transaction.
	pub registered_urls: Vec<String>,
}

/// Frontend-owned authority for atomically editing repository-local configuration and rebuilding
/// the effective superproject configuration after that edit.
pub trait ConfigurationProvider {
	#[cfg(not(target_arch = "wasm32"))]
	type ModuleWorktreeEdit: Send;

	async fn apply_init(
		&self,
		updates: &[InitConfigUpdate],
	) -> Result<InitConfigResult, SubmoduleError>;

	async fn reload(&self) -> Result<GitConfig, SubmoduleError>;

	/// Rebuild the complete effective configuration stack for an existing module repository.
	#[cfg(not(target_arch = "wasm32"))]
	async fn load_module_config(
		&self,
		git_dir: Dir,
		display_path: &Path,
	) -> Result<GitConfig, SubmoduleError>;

	/// Read and validate the module repository's local object format. This uses the native config
	/// authority so a supported config symlink can point outside the module file-store capability.
	#[cfg(not(target_arch = "wasm32"))]
	async fn module_hash_kind(
		&self,
		git_dir: Dir,
		display_path: &Path,
	) -> Result<HashKind, SubmoduleError>;

	/// Read the invocation's module-wide excludes content through the native frontend authority.
	/// The core worktree engine has no ambient authority to resolve `core.excludesFile` or the XDG
	/// default itself.
	async fn load_module_excludes(
		&self,
		config: &GitConfig,
		worktree_root: &Path,
	) -> Result<Option<String>, SubmoduleError>;

	/// Atomically update the module's repository-local `core.worktree` while preserving config
	/// symlinks and the target file's permissions. `git_dir` is the already-open module directory;
	/// namespace replacement must not redirect this mutation through `display_path`.
	#[cfg(not(target_arch = "wasm32"))]
	async fn set_module_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		worktree: &str,
	) -> Result<Self::ModuleWorktreeEdit, SubmoduleError>;

	/// Restore the repository-local config captured by [`Self::set_module_worktree`] when the mount
	/// marker's conditional publication fails. The provider must refuse to overwrite a config that no
	/// longer matches the state published by that edit.
	#[cfg(not(target_arch = "wasm32"))]
	async fn rollback_module_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		edit: Self::ModuleWorktreeEdit,
	) -> Result<(), SubmoduleError>;
}
