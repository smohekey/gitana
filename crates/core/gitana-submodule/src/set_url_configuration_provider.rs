use std::path::Path;

use cap_std::fs::Dir;
use gitana_config::GitConfig;

use crate::{
	ConfigurationProvider, DeinitConfigPublication, DeinitConfigTransition, SetUrlConfigPath,
	SetUrlValue, SubmoduleError, SubmoduleMutationLease,
};

/// Native configuration authority used by the durable set-URL state machine.
pub trait SetUrlConfigurationProvider: ConfigurationProvider {
	/// Verify that the retained superproject capabilities still occupy their public namespace.
	async fn revalidate_set_url_superproject(
		&self,
		worktree: Dir,
		worktree_root: &Path,
	) -> Result<(), SubmoduleError>;

	/// Plan the `.gitmodules` transition and return its declaration name.
	async fn plan_set_url_declaration(
		&self,
		directory: Dir,
		display_path: &Path,
		path: &str,
		url: &str,
	) -> Result<(String, DeinitConfigTransition), SubmoduleError>;

	/// Plan one repository-local URL assignment.
	async fn plan_set_url_value(
		&self,
		directory: Dir,
		display_path: &Path,
		effective: &GitConfig,
		section: &str,
		subsection: &str,
		url: &str,
	) -> Result<DeinitConfigTransition, SubmoduleError>;

	/// Verify that a published URL is still solely owned by its writable base config.
	async fn validate_set_url_effective_value(
		&self,
		directory: Dir,
		path: SetUrlConfigPath<'_>,
		effective: &GitConfig,
		key: (&str, &str),
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
	) -> Result<(), SubmoduleError>;

	/// Reserve an empty private inode for a changed URL transition.
	async fn reserve_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError>;

	/// Render and fsync one journaled URL assignment.
	async fn prepare_set_url_value(
		&self,
		directory: Dir,
		path: SetUrlConfigPath<'_>,
		value: SetUrlValue<'_>,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError>;

	/// Publish or resume one prepared URL transition.
	async fn apply_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;

	/// Restore a Windows before-image gap without consuming the prepared image.
	async fn restore_set_url_before_image(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError>;

	/// Inspect whether a journaled config target is in the Windows absent-target state.
	async fn set_url_before_image_requires_restore(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError>;

	/// Remove a pre-publication private image by its recorded identity.
	async fn discard_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError>;
}
