use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
use cap_std::fs::Dir;
use gitana_config::GitConfig;
#[cfg(not(target_arch = "wasm32"))]
use gitana_object::HashKind;

use crate::{
	DeinitConfigPublication, DeinitConfigTransition, MarkerTargetResolver, SubmoduleError,
	SubmoduleMutationLease,
};

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
///
/// Every mutating method receives a [`SubmoduleMutationLease`]. Implementations that detach work to
/// a blocking worker must retain that lease inside the worker through its complete filesystem
/// transaction so dropping the awaiting operation cannot release shared serialization early.
pub trait ConfigurationProvider: MarkerTargetResolver {
	#[cfg(not(target_arch = "wasm32"))]
	type ModuleWorktreeEdit: Send;

	/// Atomically apply repository-local initialization updates. When `active_pathspecs` is non-empty,
	/// the same transaction also records the ordered root-level `submodule.active` clone settings.
	async fn apply_init(
		&self,
		updates: &[InitConfigUpdate],
		active_pathspecs: &[String],
		lease: SubmoduleMutationLease,
	) -> Result<InitConfigResult, SubmoduleError>;

	async fn reload(&self) -> Result<GitConfig, SubmoduleError>;

	/// Rebuild the complete effective configuration stack for an existing module repository.
	#[cfg(not(target_arch = "wasm32"))]
	async fn load_module_config(
		&self,
		git_dir: Dir,
		display_path: &Path,
	) -> Result<GitConfig, SubmoduleError>;

	/// Validate that every on-disk input consulted while assembling the module's effective config
	/// remains addressable after all `selected_worktrees` are retired. Implementations must include
	/// repository/worktree layers and matched nested includes, and must bind containment to the same
	/// pinned resolution used for each read.
	#[cfg(not(target_arch = "wasm32"))]
	async fn validate_module_config_inputs_outside_worktrees(
		&self,
		git_dir: Dir,
		display_path: &Path,
		selected_worktrees: Vec<Dir>,
	) -> Result<(), SubmoduleError>;

	/// Authoritatively read and validate the module repository's local format. This uses the native
	/// config authority so a supported config symlink can point outside the module file-store
	/// capability; callers must not repeat this validation through that narrower store.
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

	/// Resolve module-wide excludes through the exact opened worktree, including after its public
	/// mount name has been displaced during deinit.
	#[cfg(not(target_arch = "wasm32"))]
	async fn load_module_excludes_at(
		&self,
		config: &GitConfig,
		worktree: Dir,
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
		lease: SubmoduleMutationLease,
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
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError>;

	/// Plan removal of the retained module repository's expected `core.worktree` value without
	/// changing the config. A different surviving value is a foreign attachment and must fail. When
	/// mounted, the exact opened checkout is supplied so config-target containment is capability-bound.
	#[cfg(not(target_arch = "wasm32"))]
	async fn plan_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		mounted_worktree: Option<Dir>,
	) -> Result<DeinitConfigTransition, SubmoduleError>;

	/// Revalidate one planned or journaled module config resolution against an exact opened
	/// checkout. Recovery may already have published the prepared config inode, so providers must
	/// accept either the planned before-image or the recorded prepared-image identity while still
	/// rejecting a changed parent or symlink chain.
	#[cfg(not(target_arch = "wasm32"))]
	async fn validate_module_deinit_target_outside_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		worktree: Dir,
		worktree_root: &Path,
	) -> Result<(), SubmoduleError>;

	/// Reserve an empty private inode for a planned module config image. The caller journals the
	/// returned identity before any config bytes are written.
	#[cfg(not(target_arch = "wasm32"))]
	async fn reserve_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError>;

	/// Fill and fsync the exact journaled module config reservation without publishing it.
	#[cfg(not(target_arch = "wasm32"))]
	async fn prepare_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError>;

	/// Restore an exact journaled module config before-image when an interrupted native
	/// publication temporarily left the public config target absent.
	#[cfg(not(target_arch = "wasm32"))]
	async fn restore_module_deinit_before_image(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;

	/// Inspect whether the exact journaled module config is in the transient absent-target state
	/// that must be restored before ordinary command setup can load repository configuration.
	#[cfg(not(target_arch = "wasm32"))]
	async fn module_deinit_before_image_requires_restore(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError>;

	/// Apply or resume an exact transition returned by [`Self::plan_module_deinit`].
	#[cfg(not(target_arch = "wasm32"))]
	async fn apply_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;

	/// Plan removal of the complete writable local `[submodule "name"]` subsection. When mounted,
	/// the exact opened checkout is supplied so config-target containment is capability-bound.
	#[cfg(not(target_arch = "wasm32"))]
	async fn plan_superproject_deinit(
		&self,
		name: &str,
		mounted_worktree: Option<Dir>,
		worktree_root: &Path,
	) -> Result<DeinitConfigTransition, SubmoduleError>;

	/// Reserve an empty private inode for a planned superproject config image.
	async fn reserve_superproject_deinit(
		&self,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError>;

	/// Fill and fsync the exact journaled superproject config reservation without publishing it.
	async fn prepare_superproject_deinit(
		&self,
		name: &str,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError>;

	/// Restore an exact journaled superproject config before-image when an interrupted native
	/// publication temporarily left the public config target absent.
	async fn restore_superproject_deinit_before_image(
		&self,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;

	/// Inspect whether the exact journaled superproject config is in the transient absent-target
	/// state that must be restored before ordinary command setup can load repository configuration.
	async fn superproject_deinit_before_image_requires_restore(
		&self,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError>;

	/// Apply or resume an exact transition returned by [`Self::plan_superproject_deinit`].
	async fn apply_superproject_deinit(
		&self,
		name: &str,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;
}
