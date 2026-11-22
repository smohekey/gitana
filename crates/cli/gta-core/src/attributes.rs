//! The attributes files git reads from outside the repository, found for the worktree library.

use std::path::Path;

use anyhow::{Result, anyhow};
use gitana_config::GitConfig;
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::HashAlgorithm;
use gitana_submodule::SubmoduleContext;
use gitana_worktree::{OutsideAttributes, WorkTree};

/// `wt`, reading the attributes files from outside the repository too, as [`outside_attributes`]
/// finds them by `wt`'s merged config. `root` is the worktree's root folder.
pub(crate) async fn with_outside_attributes<F, W, H>(
	wt: WorkTree<F, W, H>,
	root: &Path,
) -> Result<WorkTree<F, W, H>>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let config = wt.repository().effective_config().await?;
	let attributes = outside_attributes(&config, root).await?;
	Ok(wt.with_outside_attributes(attributes))
}

/// `context`, the working trees it builds (the superproject's and each submodule's) reading the
/// attributes files from outside the repository too, as its superproject's merged config finds them.
pub(crate) async fn for_submodules(context: SubmoduleContext) -> Result<SubmoduleContext> {
	let root = context
		.layout()
		.worktree_root
		.clone()
		.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;
	let attributes = outside_attributes(&context.configs().superproject, &root).await?;
	Ok(context.with_outside_attributes(attributes))
}

/// The system and global attributes files, as git reads them (see
/// [`gitana_config_native::attributes_files`]), for a worktree whose root folder is `root`; git's
/// warning printed for one that can't be read.
pub(crate) async fn outside_attributes(
	config: &GitConfig,
	root: &Path,
) -> Result<OutsideAttributes> {
	Ok(printed(
		gitana_config_native::attributes_files(config, root).await?,
	))
}

/// [`outside_attributes`] for a worktree already open as `worktree`, whose root folder was
/// `worktree_root`; see [`gitana_config_native::attributes_files_at`].
pub(crate) async fn outside_attributes_at(
	config: &GitConfig,
	worktree: cap_std::fs::Dir,
	worktree_root: &Path,
) -> Result<OutsideAttributes> {
	Ok(printed(
		gitana_config_native::attributes_files_at(config, worktree, worktree_root).await?,
	))
}

/// `files` for the worktree library, their warnings printed as git prints them.
fn printed(files: gitana_config_native::AttributesFiles) -> OutsideAttributes {
	for warning in &files.warnings {
		eprintln!("{warning}");
	}
	OutsideAttributes {
		system: files.system,
		global: files.global,
	}
}

/// `error`'s message as C's `strerror` gives it, without Rust's ` (os error N)`.
pub(crate) fn strerror(error: &std::io::Error) -> String {
	let message = error.to_string();
	match error.raw_os_error() {
		Some(code) => message
			.strip_suffix(&format!(" (os error {code})"))
			.map(str::to_owned)
			.unwrap_or(message),
		None => message,
	}
}
