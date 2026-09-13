use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, OpenOptions};
use gitana_file_store_local::{CapWorkDir, LocalFileStore};
#[cfg(windows)]
use gitana_fs_native::remove_file_if_identity;
#[cfg(not(windows))]
use gitana_fs_native::replace_if_identities;
use gitana_fs_native::{
	EntryIdentity, directory_identity, entry_identity, file_identity, remove_dir_if_identity,
	rename_noreplace_if_identity,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_repository_layout::RepositoryLayout;
use gitana_worktree::WorkTree;
use serde::{Deserialize, Serialize};

use crate::{
	ConfigurationProvider, DeinitConfigPublication, DeinitConfigTransition, DeinitFailure,
	DeinitMountMarker, DeinitOutcome, DeinitReport, DeinitRequest, DeinitSelection, DurableIdentity,
	MarkerTargetResolver, SubmoduleContext, SubmoduleDeclaration, SubmoduleError,
	SubmoduleMutationLease, declarations_by_path, validate_name, validate_path,
};

const CONTROL_DIR: &str = "gitana-submodule-deinit";
const INTENT_NAME: &str = "intent.json";
const INTENT_LOCK_NAME: &str = "intent.lock";
#[cfg(any(windows, test))]
const INTENT_PREVIOUS_NAME: &str = "intent.previous";
const UPDATE_CONTROL_DIR: &str = "gitana-submodule-update";
const INTENT_VERSION: u32 = 4;
const PRIVATE_ATTEMPTS: u64 = 100;
static PRIVATE_COUNTER: AtomicU64 = AtomicU64::new(0);
static RETIRE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Planned {
	declaration: SubmoduleDeclaration,
	recorded: String,
	core_worktree: String,
	mounted: bool,
	checkout: Option<Dir>,
	mount_identity: Option<DurableIdentity>,
	mount_marker: Option<DeinitMountMarker>,
	module: Option<Dir>,
	module_identity: Option<DurableIdentity>,
	module_transition: Option<DeinitConfigTransition>,
	module_lease: Option<SubmoduleMutationLease>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DeinitIntent {
	version: u32,
	phase: DeinitPhase,
	name: String,
	path: String,
	recorded: String,
	force: bool,
	core_worktree: String,
	module_transition: Option<DeinitConfigTransition>,
	module_publication: Option<DeinitConfigPublication>,
	super_transition: DeinitConfigTransition,
	super_publication: Option<DeinitConfigPublication>,
	parent: String,
	target: String,
	prepared: Option<String>,
	displaced: Option<String>,
	mount_identity: Option<DurableIdentity>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	mount_marker: Option<DeinitMountMarker>,
	prepared_identity: Option<DurableIdentity>,
	public_identity: DurableIdentity,
	module_identity: Option<DurableIdentity>,
	retired: Option<String>,
	rollback: Option<String>,
	retirement_location: Option<RetirementLocation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeinitPhase {
	Prepared,
	Detaching,
	MountDisplaced,
	Detached,
	RollingBack,
	RollbackMountDisplaced,
	RolledBack,
	RollbackCleaned,
	Retiring,
	Retired,
	ModuleConfigReserved,
	ModuleConfigPrepared,
	ModuleConfigApplied,
	SuperConfigReserved,
	SuperConfigPrepared,
	SuperConfigApplied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetirementLocation {
	ModuleRepository,
	WorktreeSibling,
}

/// Enumerate every administrative Git directory that may own a deinit intent for this repository.
///
/// The current worktree and common directories are included first, followed by linked-worktree
/// administrative directories in lexical order. Directory identities are deduplicated so an
/// ordinary repository is visited once. A symlinked administrative namespace is rejected rather
/// than returning a partial candidate set that could omit the sole owner of recovery state.
pub fn deinit_recovery_git_dirs(
	common: &Dir,
	current: &Dir,
	layout: &RepositoryLayout,
) -> Result<Vec<(PathBuf, Dir)>, SubmoduleError> {
	let mut candidates = Vec::new();
	let mut identities = Vec::<EntryIdentity>::new();
	push_recovery_git_dir(
		&mut candidates,
		&mut identities,
		layout.git_dir.clone(),
		current.try_clone().map_err(|source| SubmoduleError::Io {
			path: layout.git_dir.clone(),
			source,
		})?,
	)?;
	push_recovery_git_dir(
		&mut candidates,
		&mut identities,
		layout.common_dir.clone(),
		common.try_clone().map_err(|source| SubmoduleError::Io {
			path: layout.common_dir.clone(),
			source,
		})?,
	)?;

	let worktrees = match common.symlink_metadata("worktrees") {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
			let expected = EntryIdentity::from_metadata(&metadata);
			let worktrees =
				common
					.open_dir_nofollow("worktrees")
					.map_err(|source| SubmoduleError::Io {
						path: layout.common_dir.join("worktrees"),
						source,
					})?;
			let opened = directory_identity(&worktrees).map_err(|source| SubmoduleError::Io {
				path: layout.common_dir.join("worktrees"),
				source,
			})?;
			let visible =
				entry_identity(common, OsStr::new("worktrees")).map_err(|source| SubmoduleError::Io {
					path: layout.common_dir.join("worktrees"),
					source,
				})?;
			if opened != expected || visible != expected {
				return Err(SubmoduleError::RecoveryRequired(
					"linked-worktree recovery namespace changed while it was being inspected".to_owned(),
				));
			}
			worktrees
		}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"linked-worktree recovery namespace '{}' is not a real directory",
				layout.common_dir.join("worktrees").display()
			)));
		}
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: layout.common_dir.join("worktrees"),
				source,
			});
		}
	};
	let mut names = worktrees
		.entries()
		.map_err(|source| SubmoduleError::Io {
			path: layout.common_dir.join("worktrees"),
			source,
		})?
		.map(|entry| entry.map(|entry| entry.file_name()))
		.collect::<std::io::Result<Vec<_>>>()
		.map_err(|source| SubmoduleError::Io {
			path: layout.common_dir.join("worktrees"),
			source,
		})?;
	names.sort();
	for name in names {
		let display = layout.common_dir.join("worktrees").join(&name);
		let metadata = worktrees
			.symlink_metadata(&name)
			.map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?;
		if metadata.file_type().is_symlink() {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"linked-worktree recovery owner '{}' is a symbolic link",
				display.display()
			)));
		}
		if !metadata.is_dir() {
			continue;
		}
		let expected = EntryIdentity::from_metadata(&metadata);
		let directory = worktrees
			.open_dir_nofollow(&name)
			.map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?;
		let opened = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		let visible = entry_identity(&worktrees, &name).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		if opened != expected || visible != expected {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"linked-worktree recovery owner '{}' changed while it was being inspected",
				display.display()
			)));
		}
		push_recovery_git_dir(&mut candidates, &mut identities, display, directory)?;
	}
	Ok(candidates)
}

/// Report whether any worktree in the repository owns a pending deinit intent.
///
/// Malformed control entries fail closed instead of being treated as absent recovery state.
pub fn repository_has_pending_deinit(
	common: &Dir,
	current: &Dir,
	layout: &RepositoryLayout,
) -> Result<bool, SubmoduleError> {
	for (git_dir, git) in deinit_recovery_git_dirs(common, current, layout)? {
		match git.symlink_metadata(CONTROL_DIR) {
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
				return Ok(true);
			}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(format!(
					"submodule deinit control path in '{}' is not a directory",
					git_dir.display()
				)));
			}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: git_dir.join(CONTROL_DIR),
					source,
				});
			}
		}
	}
	Ok(false)
}

fn push_recovery_git_dir(
	candidates: &mut Vec<(PathBuf, Dir)>,
	identities: &mut Vec<EntryIdentity>,
	display: PathBuf,
	directory: Dir,
) -> Result<(), SubmoduleError> {
	let identity = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
		path: display.clone(),
		source,
	})?;
	if !identities.contains(&identity) {
		identities.push(identity);
		candidates.push((display, directory));
	}
	Ok(())
}

fn ensure_no_set_url_recovery_at(git: &Dir, git_dir: &Path) -> Result<(), SubmoduleError> {
	if crate::repository_has_pending_set_url(git, git_dir)? {
		return Err(SubmoduleError::RecoveryRequired(format!(
			"a pending submodule set-url in '{}' must be retried from its owning superproject",
			git_dir.display()
		)));
	}
	Ok(())
}

fn ensure_no_deinit_intent_at(git: &Dir, git_dir: &Path) -> Result<(), SubmoduleError> {
	match git.symlink_metadata(CONTROL_DIR) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
			Err(SubmoduleError::RecoveryRequired(format!(
				"a pending submodule deinit in '{}' must be completed with 'gta submodule deinit'",
				git_dir.display()
			)))
		}
		Ok(_) => Err(SubmoduleError::RecoveryRequired(format!(
			"submodule deinit control path in '{}' is not a directory",
			git_dir.display()
		))),
		Err(source) => Err(SubmoduleError::Io {
			path: git_dir.join(CONTROL_DIR),
			source,
		}),
	}
}

fn ensure_no_deinit_recovery_at(git: &Dir, git_dir: &Path) -> Result<(), SubmoduleError> {
	ensure_no_set_url_recovery_at(git, git_dir)?;
	ensure_no_deinit_intent_at(git, git_dir)
}

/// Restore config before-images needed to load a repository with a pending deinit intent.
///
/// This is a namespace repair only: it neither advances nor retires the durable intent. The
/// deinit operation remains the sole authority for completing the semantic transition.
pub async fn restore_pending_deinit_configs<C: ConfigurationProvider>(
	git: Dir,
	git_dir: &Path,
	common: Dir,
	common_dir: &Path,
	configuration: &C,
) -> Result<(), SubmoduleError> {
	match git.symlink_metadata(CONTROL_DIR) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule deinit control path is not a directory".to_owned(),
			));
		}
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: git_dir.join(CONTROL_DIR),
				source,
			});
		}
	}
	let lock =
		crate::update_operation::acquire_update_lock_with_common(&git, git_dir, &common, common_dir)?;
	lock.validate()?;
	let Some((intent, intent_identity)) = read_deinit_intent_at(&git, git_dir)? else {
		return Ok(());
	};
	validate_deinit_intent_shape(&intent)?;

	if intent.phase >= DeinitPhase::ModuleConfigPrepared
		&& let (Some(transition), Some(publication)) = (
			intent.module_transition.as_ref(),
			intent.module_publication.as_ref(),
		) && transition.changes()
	{
		ensure_active_deinit_intent_at(&git, git_dir, intent_identity)?;
		let relative = Path::new("modules").join(&intent.name);
		let display = git_dir.join(&relative);
		let module =
			open_git_subdir_nofollow(&git, &relative).map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?;
		let expected = intent.module_identity.ok_or_else(|| {
			SubmoduleError::RecoveryRequired(
				"prepared module config transition has no repository identity".to_owned(),
			)
		})?;
		if directory_identity(&module).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})? != EntryIdentity::from(expected)
		{
			return Err(SubmoduleError::RecoveryRequired(
				"module repository changed while restoring deinit configuration".to_owned(),
			));
		}
		let module_lease =
			crate::update_operation::try_acquire_submodule_config_mutation_lease(&module, &display)?;
		configuration
			.restore_module_deinit_before_image(
				module,
				&display,
				transition,
				publication,
				lock.lease().combine(module_lease),
			)
			.await?;
	}
	if intent.phase >= DeinitPhase::SuperConfigPrepared && intent.super_transition.changes() {
		ensure_active_deinit_intent_at(&git, git_dir, intent_identity)?;
		let publication = intent.super_publication.as_ref().ok_or_else(|| {
			SubmoduleError::RecoveryRequired(
				"prepared superproject config transition has no publication".to_owned(),
			)
		})?;
		configuration
			.restore_superproject_deinit_before_image(&intent.super_transition, publication, lock.lease())
			.await?;
	}
	lock.validate()
}

/// Report whether a pending deinit owns an exact transient config-publication gap.
///
/// Callers retain the repository setup lease while invoking this read-only probe. It accepts the
/// journaled before or after image, rejects foreign namespace state, and requests restoration only
/// when the active target is absent and both journaled private inodes are still exact.
pub async fn pending_deinit_configs_require_restore<C: ConfigurationProvider>(
	git: Dir,
	git_dir: &Path,
	configuration: &C,
) -> Result<bool, SubmoduleError> {
	match git.symlink_metadata(CONTROL_DIR) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule deinit control path is not a directory".to_owned(),
			));
		}
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: git_dir.join(CONTROL_DIR),
				source,
			});
		}
	}
	let Some((intent, _)) = read_deinit_intent_at(&git, git_dir)? else {
		return Ok(false);
	};
	validate_deinit_intent_shape(&intent)?;
	let mut required = false;
	if intent.phase >= DeinitPhase::ModuleConfigPrepared
		&& let (Some(transition), Some(publication)) = (
			intent.module_transition.as_ref(),
			intent.module_publication.as_ref(),
		) && transition.changes()
	{
		let relative = Path::new("modules").join(&intent.name);
		let display = git_dir.join(&relative);
		let module =
			open_git_subdir_nofollow(&git, &relative).map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?;
		let expected = intent.module_identity.ok_or_else(|| {
			SubmoduleError::RecoveryRequired(
				"prepared module config transition has no repository identity".to_owned(),
			)
		})?;
		if directory_identity(&module).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})? != EntryIdentity::from(expected)
		{
			return Err(SubmoduleError::RecoveryRequired(
				"module repository changed while inspecting deinit configuration".to_owned(),
			));
		}
		required |= configuration
			.module_deinit_before_image_requires_restore(module, &display, transition, publication)
			.await?;
	}
	if intent.phase >= DeinitPhase::SuperConfigPrepared && intent.super_transition.changes() {
		let publication = intent.super_publication.as_ref().ok_or_else(|| {
			SubmoduleError::RecoveryRequired(
				"prepared superproject config transition has no publication".to_owned(),
			)
		})?;
		required |= configuration
			.superproject_deinit_before_image_requires_restore(&intent.super_transition, publication)
			.await?;
	}
	Ok(required)
}

impl SubmoduleContext {
	/// Deinitialize selected, tracked submodules while retaining their module repositories.
	pub async fn deinit<C: ConfigurationProvider>(
		&self,
		request: &DeinitRequest,
		configuration: &C,
	) -> Result<DeinitReport, DeinitFailure> {
		match self.hash_kind {
			HashKind::Sha1 => self.deinit_typed::<Sha1, C>(request, configuration).await,
			HashKind::Sha256 => self.deinit_typed::<Sha256, C>(request, configuration).await,
		}
	}

	fn prepare_deinit_recovery(
		&self,
		intent: &DeinitIntent,
	) -> Result<(Option<Dir>, Option<SubmoduleMutationLease>), SubmoduleError> {
		validate_deinit_intent_shape(intent)?;
		let Some(expected) = intent.module_identity else {
			return Ok((None, None));
		};
		let expected = EntryIdentity::from(expected);
		let relative = Path::new("modules").join(&intent.name);
		let module_path = self.layout.git_dir.join(&relative);
		let module = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(intent.name.clone()))?;
		if directory_identity(&module).map_err(|source| SubmoduleError::Io {
			path: module_path.clone(),
			source,
		})? != expected
		{
			return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
		}
		let module_lease =
			crate::update_operation::try_acquire_submodule_config_mutation_lease(&module, &module_path)?;
		if crate::repository_has_pending_update(&module, &module_path)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule update recovery in module '{}' must be completed before deinitializing its parent",
				intent.name
			)));
		}
		let module_layout = RepositoryLayout {
			worktree_root: intent
				.mount_identity
				.map(|_| self.worktree_root().join(&intent.path)),
			git_dir: module_path.clone(),
			common_dir: module_path.clone(),
		};
		if repository_has_pending_deinit(&module, &module, &module_layout)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule deinit recovery in module '{}' must be completed before deinitializing its parent",
				intent.name
			)));
		}
		if crate::repository_has_pending_set_url_recovery(&module, &module, &module_layout)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule set-url recovery in module '{}' must be completed before deinitializing its parent",
				intent.name
			)));
		}
		module_lease.validate()?;
		self.ensure_recorded_module_identity(intent, &relative, &module)?;
		Ok((Some(module), Some(module_lease)))
	}

	async fn deinit_typed<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		request: &DeinitRequest,
		configuration: &C,
	) -> Result<DeinitReport, DeinitFailure> {
		let query = deinit_query(&request.selection).map_err(DeinitFailure::preflight)?;
		let lock = self
			.acquire_config_update_lock()
			.map_err(DeinitFailure::preflight)?;
		lock.validate().map_err(DeinitFailure::preflight)?;
		self
			.ensure_no_other_deinit_recovery()
			.map_err(DeinitFailure::preflight)?;
		self
			.ensure_no_update_recovery()
			.map_err(DeinitFailure::preflight)?;
		let pending = self
			.read_deinit_intent()
			.map_err(DeinitFailure::preflight)?;
		let mut report = DeinitReport::default();
		let recovered_path = if let Some((intent, identity)) = pending {
			let module = intent.name.clone();
			let path = intent.path.clone();
			let (retained_module, module_lease) = self
				.prepare_deinit_recovery(&intent)
				.map_err(DeinitFailure::preflight)?;
			let outcome = self
				.complete_deinit_intent::<H, C>(
					intent,
					identity,
					retained_module,
					module_lease,
					configuration,
					&lock,
				)
				.await
				.map_err(|source| DeinitFailure {
					completed: report.clone(),
					module: Some(module),
					source,
				})?;
			report.outcomes.push(outcome);
			Some(path)
		} else {
			None
		};

		let completed_recovery = report.clone();
		let preflight = |source| DeinitFailure {
			completed: completed_recovery.clone(),
			module: None,
			source,
		};
		let worktree = self.worktree::<H>().map_err(&preflight)?;
		let index = worktree
			.load_index()
			.await
			.map_err(SubmoduleError::from)
			.map_err(&preflight)?;
		let mut selected = self
			.select_gitlinks_with_prior_path(&index, &query, recovered_path.as_deref())
			.map_err(&preflight)?;
		if let Some(recovered_path) = &recovered_path {
			selected.retain(|path| path != recovered_path);
		}
		let declarations =
			declarations_by_path(self.declarations().await.map_err(&preflight)?).map_err(&preflight)?;
		if selected.is_empty() && recovered_path.is_none() {
			self
				.retire_unpublished_deinit_control()
				.map_err(&preflight)?;
			lock.validate().map_err(&preflight)?;
			return Ok(report);
		}

		// Complete structural and loss checks for every selection before changing the first module.
		let mut plan = Vec::with_capacity(selected.len());
		for path in selected {
			let declaration = declarations
				.get(&path)
				.ok_or_else(|| SubmoduleError::MissingMapping(path.clone()))
				.map_err(&preflight)?
				.clone();
			validate_name(&declaration.name).map_err(&preflight)?;
			validate_path(&declaration.path).map_err(&preflight)?;
			if index.conflict(&crate::git_path(&path)).is_some() {
				return Err(preflight(SubmoduleError::Conflicted(path)));
			}
			let recorded = index
				.entry(&crate::git_path(&path))
				.expect("selected stage-zero gitlink")
				.oid;
			let pointers = self.module_pointers(&declaration).map_err(&preflight)?;
			self
				.preflight_module_namespaces(&declaration, &pointers, configuration)
				.await
				.map_err(&preflight)?;
			let mounted = self.mount_is_attached(&declaration).map_err(&preflight)?;
			let (mount, mount_identity, mount_marker) = if mounted {
				let (parent, target, _) = self.mount_parent(&declaration.path).map_err(&preflight)?;
				let (mount, identity, marker) = self
					.capture_owned_mount(&declaration, &parent, &target, configuration)
					.await
					.map_err(&preflight)?;
				(Some(mount), Some(identity.into()), Some(marker))
			} else {
				(None, None, None)
			};
			let module_relative = Path::new("modules").join(&declaration.name);
			let module_path = self.layout.git_dir.join(&module_relative);
			let (module, module_identity, module_transition, module_lease) = match self
				.open_git_subdir_nofollow(&module_relative)
			{
				Ok(directory) => {
					let identity = directory_identity(&directory)
						.map_err(|source| SubmoduleError::Io {
							path: module_path.clone(),
							source,
						})
						.map_err(&preflight)?;
					let module_lease = crate::update_operation::try_acquire_submodule_config_mutation_lease(
						&directory,
						&module_path,
					)
					.map_err(&preflight)?;
					if crate::repository_has_pending_update(&directory, &module_path).map_err(&preflight)? {
						return Err(preflight(SubmoduleError::RecoveryRequired(format!(
							"pending submodule update recovery in module '{}' must be completed before deinitializing its parent",
							declaration.name
						))));
					}
					let module_layout = RepositoryLayout {
						worktree_root: mounted.then(|| self.worktree_root().join(&declaration.path)),
						git_dir: module_path.clone(),
						common_dir: module_path.clone(),
					};
					if repository_has_pending_deinit(&directory, &directory, &module_layout)
						.map_err(&preflight)?
					{
						return Err(preflight(SubmoduleError::RecoveryRequired(format!(
							"pending submodule deinit recovery in module '{}' must be completed before deinitializing its parent",
							declaration.name
						))));
					}
					if crate::repository_has_pending_set_url_recovery(&directory, &directory, &module_layout)
						.map_err(&preflight)?
					{
						return Err(preflight(SubmoduleError::RecoveryRequired(format!(
							"pending submodule set-url recovery in module '{}' must be completed before deinitializing its parent",
							declaration.name
						))));
					}
					self
						.ensure_module_repository_valid::<H, C>(&declaration, &directory, configuration)
						.await
						.map_err(&preflight)?;
					let transition = configuration
						.plan_module_deinit(
							directory.try_clone().map_err(|source| {
								preflight(SubmoduleError::Io {
									path: module_path.clone(),
									source,
								})
							})?,
							&module_path,
							&pointers.core_worktree,
							mount
								.as_ref()
								.map(|directory| {
									directory.try_clone().map_err(|source| {
										preflight(SubmoduleError::Io {
											path: self.worktree_root().join(&declaration.path),
											source,
										})
									})
								})
								.transpose()?,
						)
						.await
						.map_err(&preflight)?;
					self
						.ensure_deinit_module_identity(&module_relative, identity)
						.map_err(&preflight)?;
					(
						Some(directory),
						Some(identity.into()),
						Some(transition),
						Some(module_lease),
					)
				}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound && !mounted => {
					(None, None, None, None)
				}
				Err(_) => {
					return Err(preflight(SubmoduleError::InvalidRepository(
						declaration.name.clone(),
					)));
				}
			};
			if mounted && !request.force {
				let mount = mount
					.as_ref()
					.expect("mounted deinit plan retains its checkout")
					.try_clone()
					.map_err(|source| {
						preflight(SubmoduleError::Io {
							path: self.worktree_root().join(&declaration.path),
							source,
						})
					})?;
				self
					.ensure_checkout_clean_with_module::<H, C>(
						&declaration,
						mount,
						module
							.as_ref()
							.expect("a fresh mounted deinit has a module repository"),
						recorded,
						configuration,
					)
					.await
					.map_err(&preflight)?;
			}
			plan.push(Planned {
				recorded: recorded.to_string(),
				declaration,
				core_worktree: pointers.core_worktree,
				mounted,
				checkout: mount,
				mount_identity,
				mount_marker,
				module,
				module_identity,
				module_transition,
				module_lease,
			});
		}

		// All selected config targets must remain addressable after every selected checkout is
		// retired. Module A can legally resolve its config outside A while still depending on module
		// B's checkout, so own-checkout validation is insufficient for a batch operation. Retain every
		// opened checkout until this cross-product preflight is complete and perform no mutation first.
		for entry in &plan {
			let transition = entry.module_transition.as_ref();
			if let (Some(module), Some(transition)) = (entry.module.as_ref(), transition) {
				let module_path = self
					.layout
					.git_dir
					.join("modules")
					.join(&entry.declaration.name);
				for checkout_entry in &plan {
					let Some(checkout) = checkout_entry.checkout.as_ref() else {
						continue;
					};
					let checkout_path = self.worktree_root().join(&checkout_entry.declaration.path);
					configuration
						.validate_module_deinit_target_outside_worktree(
							module.try_clone().map_err(|source| {
								preflight(SubmoduleError::Io {
									path: module_path.clone(),
									source,
								})
							})?,
							&module_path,
							transition,
							None,
							checkout.try_clone().map_err(|source| {
								preflight(SubmoduleError::Io {
									path: checkout_path.clone(),
									source,
								})
							})?,
							&checkout_path,
						)
						.await
						.map_err(&preflight)?;
				}
				let selected_worktrees = plan
					.iter()
					.filter_map(|checkout_entry| {
						checkout_entry.checkout.as_ref().map(|checkout| {
							checkout.try_clone().map_err(|source| {
								preflight(SubmoduleError::Io {
									path: self.worktree_root().join(&checkout_entry.declaration.path),
									source,
								})
							})
						})
					})
					.collect::<Result<Vec<_>, _>>()?;
				configuration
					.validate_module_config_inputs_outside_worktrees(
						module.try_clone().map_err(|source| {
							preflight(SubmoduleError::Io {
								path: module_path.clone(),
								source,
							})
						})?,
						&module_path,
						selected_worktrees,
					)
					.await
					.map_err(&preflight)?;
			}
		}
		let mut super_target = None;
		for entry in &plan {
			let Some(checkout) = entry.checkout.as_ref() else {
				continue;
			};
			let checkout_path = self.worktree_root().join(&entry.declaration.path);
			let transition = configuration
				.plan_superproject_deinit(
					&entry.declaration.name,
					Some(checkout.try_clone().map_err(|source| {
						preflight(SubmoduleError::Io {
							path: checkout_path.clone(),
							source,
						})
					})?),
					&checkout_path,
				)
				.await
				.map_err(&preflight)?;
			match &super_target {
				Some(target) if target != &transition.target => {
					return Err(preflight(SubmoduleError::Configuration(
						"superproject config target changed during deinit preflight".to_owned(),
					)));
				}
				Some(_) => {}
				None => super_target = Some(transition.target),
			}
		}
		if super_target.is_none()
			&& let Some(entry) = plan.first()
		{
			super_target = Some(
				configuration
					.plan_superproject_deinit(&entry.declaration.name, None, self.worktree_root())
					.await
					.map_err(&preflight)?
					.target,
			);
		}

		// Config transitions publish complete replacement files. Two logical repositories must not
		// independently plan against the same before-image: the first publication would invalidate
		// the second transition after its checkout had already been retired. Reject final-file aliases
		// while every module guard and the common guard are still held and before any recovery side
		// effect or namespace mutation.
		let mut config_targets = Vec::with_capacity(plan.len() + 1);
		for entry in &plan {
			if let Some(transition) = &entry.module_transition {
				config_targets.push((
					format!("module '{}'", entry.declaration.name),
					transition.target.clone(),
				));
			}
		}
		if let Some(target) = super_target {
			config_targets.push(("superproject".to_owned(), target));
		}
		ensure_distinct_deinit_config_targets(&config_targets).map_err(&preflight)?;

		for entry in plan {
			let module = entry.declaration.name.clone();
			lock.validate().map_err(|source| DeinitFailure {
				completed: report.clone(),
				module: Some(module.clone()),
				source,
			})?;
			match self
				.deinit_one::<H, C>(&entry, request.force, configuration, &lock)
				.await
			{
				Ok(outcome) => report.outcomes.push(outcome),
				Err(source) => {
					return Err(DeinitFailure {
						completed: report,
						module: Some(module),
						source,
					});
				}
			}
		}
		lock.validate().map_err(|source| DeinitFailure {
			completed: report.clone(),
			module: None,
			source,
		})?;
		Ok(report)
	}

	async fn deinit_one<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned,
		force: bool,
		configuration: &C,
		lock: &crate::update_operation::UpdateLockGuard,
	) -> Result<DeinitOutcome, SubmoduleError> {
		let pointers = self.module_pointers(&entry.declaration)?;
		if pointers.core_worktree != entry.core_worktree {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule attachment pointers changed after deinit preflight".to_owned(),
			));
		}
		self
			.preflight_module_namespaces(&entry.declaration, &pointers, configuration)
			.await?;
		if self.mount_is_attached(&entry.declaration)? != entry.mounted {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		if !entry.mounted {
			self.ensure_mount_directory(&entry.declaration.path, true)?;
		}
		let (preplan_public_identity, preplan_checkout) = {
			let (parent, target, _) = self.mount_parent(&entry.declaration.path)?;
			if entry.mounted {
				let marker = entry.mount_marker.as_ref().ok_or_else(|| {
					SubmoduleError::RecoveryRequired(
						"mounted deinit plan has no exact mount marker".to_owned(),
					)
				})?;
				let expected = entry
					.mount_identity
					.ok_or_else(|| {
						SubmoduleError::RecoveryRequired("mounted deinit plan has no mount identity".to_owned())
					})?
					.into();
				let (checkout, current, current_marker) = self
					.capture_owned_mount(&entry.declaration, &parent, &target, configuration)
					.await?;
				if current != expected || &current_marker != marker {
					return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
				}
				(current, Some(checkout))
			} else {
				let identity = entry_identity(&parent, &target).map_err(|source| SubmoduleError::Io {
					path: self.worktree_root().join(&entry.declaration.path),
					source,
				})?;
				self.ensure_empty_identity(&parent, &target, identity, &entry.declaration.path)?;
				(identity, None)
			}
		};
		let module_relative = Path::new("modules").join(&entry.declaration.name);
		let module_path = self.layout.git_dir.join(&module_relative);
		let module = entry
			.module
			.as_ref()
			.map(|directory| {
				directory.try_clone().map_err(|source| SubmoduleError::Io {
					path: module_path.clone(),
					source,
				})
			})
			.transpose()?;
		if let (Some(directory), Some(identity)) = (&module, entry.module_identity) {
			self.ensure_recorded_planned_module_identity(
				&entry.declaration.name,
				&module_relative,
				directory,
				identity.into(),
			)?;
		}
		let module_transition = entry.module_transition.clone();
		let module_identity = entry.module_identity;
		let checkout_path = self.worktree_root().join(&entry.declaration.path);
		let super_transition = configuration
			.plan_superproject_deinit(&entry.declaration.name, preplan_checkout, &checkout_path)
			.await?;
		if let (Some(directory), Some(identity)) = (&module, entry.module_identity) {
			self.ensure_recorded_planned_module_identity(
				&entry.declaration.name,
				&module_relative,
				directory,
				identity.into(),
			)?;
		}

		let (parent, target, parent_display) = self.mount_parent(&entry.declaration.path)?;
		let (
			prepared,
			displaced,
			retired,
			rollback,
			mount_identity,
			prepared_identity,
			public_identity,
		) = if entry.mounted {
			let marker = entry.mount_marker.as_ref().ok_or_else(|| {
				SubmoduleError::RecoveryRequired("mounted deinit plan has no exact mount marker".to_owned())
			})?;
			let mount_identity = self
				.revalidate_owned_mount(
					&entry.declaration,
					&parent,
					&target,
					preplan_public_identity,
					marker,
					configuration,
				)
				.await?;
			let prepared = self.create_private_empty_directory(&parent, "prepared", &parent_display)?;
			let prepared_identity =
				entry_identity(&parent, &prepared).map_err(|source| SubmoduleError::Io {
					path: parent_display.join(&prepared),
					source,
				})?;
			let displaced = self.private_absent_name(&parent, "displaced", &parent_display)?;
			let retired = self.private_retirement_name(
				&parent,
				module.as_ref().expect("a mounted module has a repository"),
				&parent_display,
				&module_path,
			)?;
			let rollback = Some(self.private_absent_name(&parent, "rollback", &parent_display)?);
			(
				Some(os_string(&prepared)?),
				Some(os_string(&displaced)?),
				Some(os_string(&retired)?),
				rollback.as_deref().map(os_string).transpose()?,
				Some(mount_identity.into()),
				Some(prepared_identity.into()),
				prepared_identity.into(),
			)
		} else {
			let public_identity = preplan_public_identity;
			self.ensure_empty_identity(&parent, &target, public_identity, &entry.declaration.path)?;
			(None, None, None, None, None, None, public_identity.into())
		};
		let intent = DeinitIntent {
			version: INTENT_VERSION,
			phase: DeinitPhase::Prepared,
			name: entry.declaration.name.clone(),
			path: entry.declaration.path.clone(),
			recorded: entry.recorded.clone(),
			force,
			core_worktree: entry.core_worktree.clone(),
			module_transition,
			module_publication: None,
			super_transition,
			super_publication: None,
			parent: path_string(
				Path::new(&entry.declaration.path)
					.parent()
					.unwrap_or(Path::new("")),
			)?,
			target: os_string(&target)?,
			prepared,
			displaced,
			mount_identity,
			mount_marker: entry.mount_marker.clone(),
			prepared_identity,
			public_identity,
			module_identity,
			retired,
			rollback,
			retirement_location: None,
		};
		self.validate_deinit_intent(&intent, entry)?;
		let identity = self.publish_deinit_intent(&intent)?;
		self
			.complete_deinit_intent::<H, C>(
				intent,
				identity,
				module,
				entry.module_lease.clone(),
				configuration,
				lock,
			)
			.await
	}

	async fn complete_deinit_intent<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		mut intent: DeinitIntent,
		mut intent_identity: EntryIdentity,
		retained_module: Option<Dir>,
		module_lease: Option<SubmoduleMutationLease>,
		configuration: &C,
		lock: &crate::update_operation::UpdateLockGuard,
	) -> Result<DeinitOutcome, SubmoduleError> {
		validate_deinit_intent_shape(&intent)?;
		self.ensure_active_deinit_intent(intent_identity)?;
		let parent_path = Path::new(&intent.parent);
		let parent = self.open_work_subdir_nofollow(parent_path)?;
		let parent_display = self.worktree_root().join(parent_path);
		let target = OsString::from(&intent.target);
		let module_relative = Path::new("modules").join(&intent.name);
		let module_path = self.layout.git_dir.join(&module_relative);
		let module = self.pinned_module(&intent, retained_module)?;
		let module_lease = match (module.as_ref(), module_lease) {
			(Some(_), Some(lease)) => Some(lease),
			(None, None) => None,
			(Some(_), None) => {
				return Err(SubmoduleError::RecoveryRequired(
					"module repository is not protected by its config mutation lock".to_owned(),
				));
			}
			(None, Some(_)) => {
				return Err(SubmoduleError::RecoveryRequired(
					"module config mutation lock has no pinned repository".to_owned(),
				));
			}
		};
		self
			.normalize_legacy_mount_marker_intent(
				&mut intent,
				&mut intent_identity,
				&parent,
				&target,
				module.as_ref(),
				configuration,
			)
			.await?;
		#[cfg(not(windows))]
		self.normalize_legacy_unix_mount_intent(
			&mut intent,
			&mut intent_identity,
			&parent,
			&target,
			&parent_display,
		)?;
		validate_deinit_intent_shape(&intent)?;
		self.ensure_active_deinit_intent(intent_identity)?;
		if intent.phase >= DeinitPhase::ModuleConfigPrepared
			&& let (Some(module), Some(transition), Some(publication)) = (
				module.as_ref(),
				intent.module_transition.as_ref(),
				intent.module_publication.as_ref(),
			) && transition.changes()
		{
			self.ensure_active_deinit_intent(intent_identity)?;
			self.ensure_recorded_module_identity(&intent, &module_relative, module)?;
			configuration
				.restore_module_deinit_before_image(
					module.try_clone().map_err(|source| SubmoduleError::Io {
						path: module_path.clone(),
						source,
					})?,
					&module_path,
					transition,
					publication,
					lock.lease().combine(
						module_lease
							.as_ref()
							.expect("a pinned module has its config mutation lease")
							.clone(),
					),
				)
				.await?;
		}
		if intent.phase >= DeinitPhase::SuperConfigPrepared && intent.super_transition.changes() {
			self.ensure_active_deinit_intent(intent_identity)?;
			let publication = intent.super_publication.as_ref().ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"prepared superproject config transition has no publication".to_owned(),
				)
			})?;
			configuration
				.restore_superproject_deinit_before_image(
					&intent.super_transition,
					publication,
					lock.lease(),
				)
				.await?;
		}
		if let Some(module) = &module {
			self
				.ensure_module_repository_valid::<H, C>(
					&SubmoduleDeclaration {
						name: intent.name.clone(),
						path: intent.path.clone(),
						url: None,
						branch: None,
						update: None,
						shallow: None,
					},
					module,
					configuration,
				)
				.await?;
		}

		if let (Some(mount), Some(prepared_identity), Some(prepared), Some(displaced), Some(retired)) = (
			intent.mount_identity,
			intent.prepared_identity,
			intent.prepared.as_deref(),
			intent.displaced.as_deref(),
			intent.retired.as_deref(),
		) {
			let mount: EntryIdentity = mount.into();
			let empty: EntryIdentity = prepared_identity.into();
			let prepared = OsString::from(prepared);
			let displaced = OsString::from(displaced);
			let retired = OsString::from(retired);

			if matches!(
				intent.phase,
				DeinitPhase::RollingBack
					| DeinitPhase::RollbackMountDisplaced
					| DeinitPhase::RolledBack
					| DeinitPhase::RollbackCleaned
			) {
				self
					.complete_mount_rollback(
						&mut intent,
						&mut intent_identity,
						&parent,
						&target,
						empty,
						&displaced,
						mount,
						&parent_display,
						configuration,
					)
					.await?;
				return Err(SubmoduleError::LocalModifications(intent.path));
			}

			if intent.phase <= DeinitPhase::Detached {
				self.ensure_mount_detached(
					&mut intent,
					&mut intent_identity,
					&parent,
					&target,
					&prepared,
					&displaced,
					empty,
					mount,
					&parent_display,
				)?;
				self.ensure_empty_identity(&parent, &target, empty, &intent.path)?;
				self
					.ensure_displaced_owned(&intent, &parent, &displaced, mount, configuration)
					.await?;

				if !intent.force
					&& let Err(source) = self
						.ensure_displaced_clean::<H, C>(
							&intent,
							&parent,
							&displaced,
							module
								.as_ref()
								.ok_or_else(|| SubmoduleError::InvalidRepository(intent.name.clone()))?,
							configuration,
						)
						.await
				{
					if matches!(&source, SubmoduleError::LocalModifications(_)) {
						self
							.complete_mount_rollback(
								&mut intent,
								&mut intent_identity,
								&parent,
								&target,
								empty,
								&displaced,
								mount,
								&parent_display,
								configuration,
							)
							.await?;
					}
					return Err(source);
				}
			}

			let module = module
				.as_ref()
				.ok_or_else(|| SubmoduleError::InvalidRepository(intent.name.clone()))?;
			self
				.ensure_checkout_retired(
					&mut intent,
					&mut intent_identity,
					&parent,
					&displaced,
					&retired,
					mount,
					module,
					&module_relative,
					&module_path,
					&parent_display,
					configuration,
				)
				.await?;
		} else {
			self.ensure_empty_identity(
				&parent,
				&target,
				intent.public_identity.into(),
				&intent.path,
			)?;
			if intent.phase < DeinitPhase::Retired {
				intent_identity =
					self.advance_deinit_intent(&mut intent, intent_identity, DeinitPhase::Retired)?;
			}
		}

		self.ensure_current_public_mount_empty(&intent)?;

		if let Some(transition) = intent.module_transition.clone() {
			let module = module
				.as_ref()
				.ok_or_else(|| SubmoduleError::InvalidRepository(intent.name.clone()))?;
			self.ensure_recorded_module_identity(&intent, &module_relative, module)?;
			if transition.changes() && intent.phase < DeinitPhase::ModuleConfigReserved {
				let publication = configuration
					.reserve_module_deinit(
						module.try_clone().map_err(|source| SubmoduleError::Io {
							path: module_path.clone(),
							source,
						})?,
						&module_path,
						&intent.core_worktree,
						&transition,
						lock.lease().combine(
							module_lease
								.as_ref()
								.expect("a pinned module has its config mutation lease")
								.clone(),
						),
					)
					.await?
					.ok_or_else(|| {
						SubmoduleError::RecoveryRequired(
							"changed module config transition produced no reservation".to_owned(),
						)
					})?;
				intent.module_publication = Some(publication);
				intent_identity = self.advance_deinit_intent(
					&mut intent,
					intent_identity,
					DeinitPhase::ModuleConfigReserved,
				)?;
			}
			if transition.changes() && intent.phase < DeinitPhase::ModuleConfigPrepared {
				let publication = intent.module_publication.as_ref().ok_or_else(|| {
					SubmoduleError::RecoveryRequired(
						"reserved module config transition has no publication".to_owned(),
					)
				})?;
				configuration
					.prepare_module_deinit(
						module.try_clone().map_err(|source| SubmoduleError::Io {
							path: module_path.clone(),
							source,
						})?,
						&module_path,
						&intent.core_worktree,
						&transition,
						publication,
						lock.lease().combine(
							module_lease
								.as_ref()
								.expect("a pinned module has its config mutation lease")
								.clone(),
						),
					)
					.await?;
				intent_identity = self.advance_deinit_intent(
					&mut intent,
					intent_identity,
					DeinitPhase::ModuleConfigPrepared,
				)?;
			}
			configuration
				.apply_module_deinit(
					module.try_clone().map_err(|source| SubmoduleError::Io {
						path: module_path.clone(),
						source,
					})?,
					&module_path,
					&intent.core_worktree,
					&transition,
					intent.module_publication.as_ref(),
					lock.lease().combine(
						module_lease
							.as_ref()
							.expect("a pinned module has its config mutation lease")
							.clone(),
					),
				)
				.await?;
			self.ensure_recorded_module_identity(&intent, &module_relative, module)?;
		}
		if intent.phase < DeinitPhase::ModuleConfigApplied {
			intent_identity = self.advance_deinit_intent(
				&mut intent,
				intent_identity,
				DeinitPhase::ModuleConfigApplied,
			)?;
		}
		let super_transition = intent.super_transition.clone();
		if super_transition.changes() && intent.phase < DeinitPhase::SuperConfigReserved {
			let publication = configuration
				.reserve_superproject_deinit(&super_transition, lock.lease())
				.await?
				.ok_or_else(|| {
					SubmoduleError::RecoveryRequired(
						"changed superproject config transition produced no reservation".to_owned(),
					)
				})?;
			intent.super_publication = Some(publication);
			intent_identity = self.advance_deinit_intent(
				&mut intent,
				intent_identity,
				DeinitPhase::SuperConfigReserved,
			)?;
		}
		if super_transition.changes() && intent.phase < DeinitPhase::SuperConfigPrepared {
			let publication = intent.super_publication.as_ref().ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"reserved superproject config transition has no publication".to_owned(),
				)
			})?;
			configuration
				.prepare_superproject_deinit(&intent.name, &super_transition, publication, lock.lease())
				.await?;
			intent_identity = self.advance_deinit_intent(
				&mut intent,
				intent_identity,
				DeinitPhase::SuperConfigPrepared,
			)?;
		}
		configuration
			.apply_superproject_deinit(
				&intent.name,
				&super_transition,
				intent.super_publication.as_ref(),
				lock.lease(),
			)
			.await?;
		if let Some(module) = &module {
			self.ensure_recorded_module_identity(&intent, &module_relative, module)?;
		}
		if intent.phase < DeinitPhase::SuperConfigApplied {
			intent_identity = self.advance_deinit_intent(
				&mut intent,
				intent_identity,
				DeinitPhase::SuperConfigApplied,
			)?;
		}
		if let Some(mount) = intent.mount_identity {
			self
				.ensure_retired_mount_owned(
					&intent,
					&parent,
					module
						.as_ref()
						.ok_or_else(|| SubmoduleError::InvalidRepository(intent.name.clone()))?,
					mount.into(),
					configuration,
				)
				.await?;
		}
		self.ensure_current_public_mount_empty(&intent)?;
		let unregistered = intent.super_transition.changes();
		self.clear_deinit_intent(intent_identity)?;
		Ok(DeinitOutcome {
			name: intent.name.clone(),
			path: intent.path.clone(),
			cleared: intent.mount_identity.is_some(),
			unregistered,
		})
	}

	#[allow(clippy::too_many_arguments)]
	fn ensure_mount_detached(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		target: &OsStr,
		prepared: &OsStr,
		displaced: &OsStr,
		empty: EntryIdentity,
		mount: EntryIdentity,
		parent_display: &Path,
	) -> Result<(), SubmoduleError> {
		if intent.phase == DeinitPhase::Prepared {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::Detaching)?;
		}
		match (
			identity_if_present(parent, target)?,
			identity_if_present(parent, prepared)?,
			identity_if_present(parent, displaced)?,
		) {
			(Some(current_mount), Some(current_empty), None)
				if intent.phase == DeinitPhase::Detaching
					&& current_mount == mount
					&& current_empty == empty =>
			{
				rename_noreplace_if_identity(parent, target, mount, parent, displaced).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(target),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
				*intent_identity =
					self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::MountDisplaced)?;
			}
			(None, Some(current_empty), Some(current_mount))
				if intent.phase <= DeinitPhase::MountDisplaced
					&& current_empty == empty
					&& current_mount == mount =>
			{
				if intent.phase < DeinitPhase::MountDisplaced {
					*intent_identity =
						self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::MountDisplaced)?;
				}
			}
			(None, Some(current_empty), Some(raced_mount))
				if intent.phase <= DeinitPhase::MountDisplaced
					&& current_empty == empty
					&& raced_mount != mount =>
			{
				rename_noreplace_if_identity(parent, displaced, raced_mount, parent, target).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(displaced),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
				return Err(SubmoduleError::ForeignMount(intent.path.clone()));
			}
			(Some(raced_empty), None, Some(current_mount))
				if intent.phase == DeinitPhase::MountDisplaced
					&& raced_empty != empty
					&& current_mount == mount =>
			{
				rename_noreplace_if_identity(parent, target, raced_empty, parent, prepared).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(target),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
				rename_noreplace_if_identity(parent, displaced, mount, parent, target).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(displaced),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
				return Err(SubmoduleError::ForeignMount(intent.path.clone()));
			}
			(None, Some(raced_empty), Some(current_mount))
				if intent.phase == DeinitPhase::MountDisplaced
					&& raced_empty != empty
					&& current_mount == mount =>
			{
				rename_noreplace_if_identity(parent, displaced, mount, parent, target).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(displaced),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
				return Err(SubmoduleError::ForeignMount(intent.path.clone()));
			}
			(Some(current_empty), None, Some(current_mount))
				if current_empty == empty && current_mount == mount =>
			{
				if intent.phase < DeinitPhase::Detached {
					*intent_identity =
						self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::Detached)?;
				}
				return Ok(());
			}
			_ => return Err(SubmoduleError::ForeignMount(intent.path.clone())),
		}
		if identity_if_present(parent, target)?.is_none() {
			rename_noreplace_if_identity(parent, prepared, empty, parent, target).map_err(|source| {
				SubmoduleError::Io {
					path: parent_display.join(target),
					source,
				}
			})?;
			sync_directory(parent, parent_display)?;
		}
		if intent.phase < DeinitPhase::Detached {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::Detached)?;
		}
		Ok(())
	}

	#[cfg(not(windows))]
	fn normalize_legacy_unix_mount_intent(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		target: &OsStr,
		parent_display: &Path,
	) -> Result<(), SubmoduleError> {
		let (Some(prepared), Some(displaced), Some(mount), Some(empty)) = (
			intent.prepared.clone(),
			intent.displaced.clone(),
			intent.mount_identity.map(EntryIdentity::from),
			intent.prepared_identity.map(EntryIdentity::from),
		) else {
			return Ok(());
		};
		if prepared != displaced {
			return Ok(());
		}

		let legacy = OsString::from(&prepared);
		let public = identity_if_present(parent, target)?;
		let private = identity_if_present(parent, &legacy)?;
		let mut phase = intent.phase;
		let (prepared, displaced) = match (public, private) {
			(Some(current_mount), Some(current_empty))
				if current_mount == mount && current_empty == empty && phase < DeinitPhase::RollingBack =>
			{
				(
					legacy,
					self.private_absent_name(parent, "displaced", parent_display)?,
				)
			}
			(Some(current_empty), Some(current_mount))
				if current_empty == empty && current_mount == mount =>
			{
				if phase < DeinitPhase::Detached {
					phase = DeinitPhase::Detached;
				}
				(
					self.private_absent_name(parent, "prepared", parent_display)?,
					legacy,
				)
			}
			(Some(current_mount), Some(current_empty))
				if current_mount == mount
					&& current_empty == empty
					&& phase >= DeinitPhase::RollingBack =>
			{
				self.ensure_empty_identity(parent, &legacy, empty, &intent.path)?;
				remove_dir_if_identity(parent, &legacy, empty).map_err(|source| SubmoduleError::Io {
					path: parent_display.join(&legacy),
					source,
				})?;
				sync_directory(parent, parent_display)?;
				phase = DeinitPhase::RollbackCleaned;
				(
					self.private_absent_name(parent, "prepared", parent_display)?,
					self.private_absent_name(parent, "displaced", parent_display)?,
				)
			}
			(Some(current_mount), None) if current_mount == mount && phase >= DeinitPhase::RolledBack => {
				phase = DeinitPhase::RollbackCleaned;
				(
					self.private_absent_name(parent, "prepared", parent_display)?,
					self.private_absent_name(parent, "displaced", parent_display)?,
				)
			}
			(Some(current_empty), None) if current_empty == empty && phase >= DeinitPhase::Retiring => (
				legacy,
				self.private_absent_name(parent, "displaced", parent_display)?,
			),
			_ => return Err(SubmoduleError::ForeignMount(intent.path.clone())),
		};

		intent.prepared = Some(os_string(&prepared)?);
		intent.displaced = Some(os_string(&displaced)?);
		intent.rollback = Some(os_string(&self.private_absent_name(
			parent,
			"rollback",
			parent_display,
		)?)?);
		*intent_identity = self.advance_deinit_intent(intent, *intent_identity, phase)?;
		Ok(())
	}

	async fn normalize_legacy_mount_marker_intent<R: MarkerTargetResolver>(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		target: &OsStr,
		module: Option<&Dir>,
		configuration: &R,
	) -> Result<(), SubmoduleError> {
		let Some(expected) = intent.mount_identity.map(EntryIdentity::from) else {
			return Ok(());
		};
		if intent.mount_marker.is_some() {
			return Ok(());
		}

		let mut names = vec![target.to_owned()];
		for name in [
			intent.prepared.as_deref(),
			intent.displaced.as_deref(),
			intent.retired.as_deref(),
		]
		.into_iter()
		.flatten()
		{
			names.push(OsString::from(name));
		}
		names.sort();
		names.dedup();

		let mut marker = None;
		for name in names {
			if let Some(current) = self
				.capture_recorded_mount_marker_at(
					intent,
					parent,
					&name,
					expected,
					name == target,
					configuration,
				)
				.await?
				&& marker.replace(current).is_some()
			{
				return Err(SubmoduleError::RecoveryRequired(
					"recorded deinit mount appears at more than one journaled name".to_owned(),
				));
			}
		}
		if let (Some(module), Some(retired)) = (module, intent.retired.as_deref())
			&& let Some(current) = self
				.capture_recorded_mount_marker_at(
					intent,
					module,
					OsStr::new(retired),
					expected,
					false,
					configuration,
				)
				.await?
			&& marker.replace(current).is_some()
		{
			return Err(SubmoduleError::RecoveryRequired(
				"recorded deinit mount appears at more than one journaled name".to_owned(),
			));
		}
		intent.mount_marker =
			Some(marker.ok_or_else(|| SubmoduleError::ForeignMount(intent.path.clone()))?);
		let phase = intent.phase;
		*intent_identity = self.advance_deinit_intent(intent, *intent_identity, phase)?;
		Ok(())
	}

	async fn capture_recorded_mount_marker_at<R: MarkerTargetResolver>(
		&self,
		intent: &DeinitIntent,
		directory: &Dir,
		name: &OsStr,
		expected: EntryIdentity,
		at_public_path: bool,
		configuration: &R,
	) -> Result<Option<DeinitMountMarker>, SubmoduleError> {
		if identity_if_present(directory, name)? != Some(expected) {
			return Ok(None);
		}
		let mount = directory
			.open_dir_nofollow(name)
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&intent.path),
				source,
			})?;
		if directory_identity(&mount).map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().join(&intent.path),
			source,
		})? != expected
		{
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		let declaration = SubmoduleDeclaration {
			name: intent.name.clone(),
			path: intent.path.clone(),
			url: None,
			branch: None,
			update: None,
			shallow: None,
		};
		let marker = if at_public_path {
			self
				.ensure_mount_marker_owned(&declaration, &mount, configuration)
				.await?
		} else {
			// A legacy intent already pins both the mount and module repository identities. Once the
			// mount has moved, resolving a relative marker from its former public path is no longer
			// meaningful: components inside the checkout may have moved with it. Capture the exact marker
			// through the recorded mount capability, then persist that snapshot before further mutation.
			self.read_mount_marker(&declaration, &mount)?
		};
		if identity_if_present(directory, name)? != Some(expected) {
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		Ok(Some(marker))
	}

	#[allow(clippy::too_many_arguments)]
	async fn complete_mount_rollback<C: ConfigurationProvider>(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		target: &OsStr,
		empty: EntryIdentity,
		displaced: &OsStr,
		mount: EntryIdentity,
		parent_display: &Path,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		if intent.phase == DeinitPhase::RollbackCleaned {
			let empty_name = intent.rollback.as_deref().ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"deinit rollback has no recorded empty-directory name".to_owned(),
				)
			})?;
			if identity_if_present(parent, OsStr::new(empty_name))?.is_some() {
				return Err(SubmoduleError::RecoveryRequired(
					"cleaned rollback still has a private empty directory".to_owned(),
				));
			}
			self
				.ensure_current_public_mount_owned(intent, mount, configuration)
				.await?;
			self.clear_deinit_intent(*intent_identity)?;
			return Ok(());
		}
		if intent.phase < DeinitPhase::RollingBack {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::RollingBack)?;
		}

		let empty_name = {
			let rollback = intent.rollback.clone().ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"deinit rollback has no recorded empty-directory name".to_owned(),
				)
			})?;
			let rollback = OsString::from(rollback);
			match (
				identity_if_present(parent, target)?,
				identity_if_present(parent, displaced)?,
				identity_if_present(parent, &rollback)?,
			) {
				(Some(current_empty), Some(current_mount), None)
					if intent.phase == DeinitPhase::RollingBack
						&& current_empty == empty
						&& current_mount == mount =>
				{
					self.ensure_empty_identity(parent, target, empty, &intent.path)?;
					rename_noreplace_if_identity(parent, target, empty, parent, &rollback).map_err(
						|source| SubmoduleError::Io {
							path: parent_display.join(target),
							source,
						},
					)?;
					sync_directory(parent, parent_display)?;
					*intent_identity = self.advance_deinit_intent(
						intent,
						*intent_identity,
						DeinitPhase::RollbackMountDisplaced,
					)?;
				}
				(None, Some(current_mount), Some(current_empty))
					if intent.phase <= DeinitPhase::RollbackMountDisplaced
						&& current_mount == mount
						&& current_empty == empty =>
				{
					if intent.phase < DeinitPhase::RollbackMountDisplaced {
						*intent_identity = self.advance_deinit_intent(
							intent,
							*intent_identity,
							DeinitPhase::RollbackMountDisplaced,
						)?;
					}
				}
				(None, Some(current_mount), Some(raced_empty))
					if intent.phase <= DeinitPhase::RollbackMountDisplaced
						&& current_mount == mount
						&& raced_empty != empty =>
				{
					rename_noreplace_if_identity(parent, &rollback, raced_empty, parent, target).map_err(
						|source| SubmoduleError::Io {
							path: parent_display.join(&rollback),
							source,
						},
					)?;
					sync_directory(parent, parent_display)?;
					return Err(SubmoduleError::ForeignMount(intent.path.clone()));
				}
				(Some(raced_mount), None, Some(current_empty))
					if intent.phase == DeinitPhase::RollbackMountDisplaced
						&& raced_mount != mount
						&& current_empty == empty =>
				{
					rename_noreplace_if_identity(parent, target, raced_mount, parent, displaced).map_err(
						|source| SubmoduleError::Io {
							path: parent_display.join(target),
							source,
						},
					)?;
					sync_directory(parent, parent_display)?;
					return Err(SubmoduleError::ForeignMount(intent.path.clone()));
				}
				(Some(current_mount), None, Some(current_empty))
					if current_mount == mount && current_empty == empty =>
				{
					return self
						.finish_mount_rollback(
							intent,
							intent_identity,
							parent,
							target,
							&rollback,
							empty,
							mount,
							parent_display,
							configuration,
						)
						.await;
				}
				(Some(current_mount), None, None)
					if intent.phase >= DeinitPhase::RolledBack && current_mount == mount =>
				{
					return self
						.finish_mount_rollback(
							intent,
							intent_identity,
							parent,
							target,
							&rollback,
							empty,
							mount,
							parent_display,
							configuration,
						)
						.await;
				}
				_ => return Err(SubmoduleError::ForeignMount(intent.path.clone())),
			}
			if identity_if_present(parent, target)?.is_none() {
				rename_noreplace_if_identity(parent, displaced, mount, parent, target).map_err(
					|source| SubmoduleError::Io {
						path: parent_display.join(target),
						source,
					},
				)?;
				sync_directory(parent, parent_display)?;
			}
			rollback
		};

		self
			.finish_mount_rollback(
				intent,
				intent_identity,
				parent,
				target,
				&empty_name,
				empty,
				mount,
				parent_display,
				configuration,
			)
			.await
	}

	#[allow(clippy::too_many_arguments)]
	async fn finish_mount_rollback<C: ConfigurationProvider>(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		target: &OsStr,
		empty_name: &OsStr,
		empty: EntryIdentity,
		mount: EntryIdentity,
		parent_display: &Path,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		self
			.ensure_displaced_owned(intent, parent, target, mount, configuration)
			.await?;
		self
			.ensure_current_public_mount_owned(intent, mount, configuration)
			.await?;
		if intent.phase < DeinitPhase::RolledBack {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::RolledBack)?;
		}
		match identity_if_present(parent, empty_name)? {
			Some(current) if current == empty => {
				self.ensure_empty_identity(parent, empty_name, empty, &intent.path)?;
				remove_dir_if_identity(parent, empty_name, empty).map_err(|source| SubmoduleError::Io {
					path: parent_display.join(empty_name),
					source,
				})?;
				sync_directory(parent, parent_display)?;
			}
			None => {}
			Some(_) => return Err(SubmoduleError::ForeignMount(intent.path.clone())),
		}
		if intent.phase < DeinitPhase::RollbackCleaned {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::RollbackCleaned)?;
		}
		self
			.ensure_current_public_mount_owned(intent, mount, configuration)
			.await?;
		self.clear_deinit_intent(*intent_identity)?;
		Ok(())
	}

	#[allow(clippy::too_many_arguments)]
	async fn ensure_checkout_retired<C: ConfigurationProvider>(
		&self,
		intent: &mut DeinitIntent,
		intent_identity: &mut EntryIdentity,
		parent: &Dir,
		displaced: &OsStr,
		retired: &OsStr,
		mount: EntryIdentity,
		module: &Dir,
		module_relative: &Path,
		module_path: &Path,
		parent_display: &Path,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		if intent.phase < DeinitPhase::Retiring {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::Retiring)?;
		}
		self.ensure_recorded_module_identity(intent, module_relative, module)?;

		let source = identity_if_present(parent, displaced)?;
		let module_retired = identity_if_present(module, retired)?;
		let sibling_retired = identity_if_present(parent, retired)?;
		let location = match (source, module_retired, sibling_retired) {
			(None, Some(identity), None) if identity == mount => RetirementLocation::ModuleRepository,
			(None, None, Some(identity)) if identity == mount => RetirementLocation::WorktreeSibling,
			(Some(identity), None, None)
				if intent.phase == DeinitPhase::Retiring && identity == mount =>
			{
				self
					.ensure_displaced_owned(intent, parent, displaced, mount, configuration)
					.await?;
				match rename_noreplace_if_identity(parent, displaced, mount, module, retired) {
					Ok(()) => RetirementLocation::ModuleRepository,
					Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
						rename_noreplace_if_identity(parent, displaced, mount, parent, retired).map_err(
							|source| SubmoduleError::Io {
								path: parent_display.join(retired),
								source,
							},
						)?;
						RetirementLocation::WorktreeSibling
					}
					Err(source) => {
						return Err(SubmoduleError::Io {
							path: parent_display.join(displaced),
							source,
						});
					}
				}
			}
			_ => return Err(SubmoduleError::ForeignMount(intent.path.clone())),
		};

		if intent
			.retirement_location
			.is_some_and(|recorded| recorded != location)
		{
			return Err(SubmoduleError::RecoveryRequired(
				"retired checkout does not match its recorded location".to_owned(),
			));
		}
		match location {
			RetirementLocation::ModuleRepository => {
				self
					.ensure_displaced_owned(intent, module, retired, mount, configuration)
					.await?;
				sync_destination_then_source(
					|| sync_directory(module, module_path),
					|| sync_directory(parent, parent_display),
				)?;
			}
			RetirementLocation::WorktreeSibling => {
				self
					.ensure_displaced_owned(intent, parent, retired, mount, configuration)
					.await?;
				sync_directory(parent, parent_display)?;
			}
		}
		self.ensure_recorded_module_identity(intent, module_relative, module)?;
		intent.retirement_location = Some(location);
		if intent.phase < DeinitPhase::Retired {
			*intent_identity =
				self.advance_deinit_intent(intent, *intent_identity, DeinitPhase::Retired)?;
		}
		Ok(())
	}

	async fn ensure_retired_mount_owned<R: MarkerTargetResolver>(
		&self,
		intent: &DeinitIntent,
		parent: &Dir,
		module: &Dir,
		mount: EntryIdentity,
		configuration: &R,
	) -> Result<(), SubmoduleError> {
		let retired = intent.retired.as_deref().ok_or_else(|| {
			SubmoduleError::RecoveryRequired(
				"mounted deinit intent has no checkout retirement name".to_owned(),
			)
		})?;
		match intent.retirement_location {
			Some(RetirementLocation::ModuleRepository) => {
				self
					.ensure_displaced_owned(intent, module, OsStr::new(retired), mount, configuration)
					.await
			}
			Some(RetirementLocation::WorktreeSibling) => {
				self
					.ensure_displaced_owned(intent, parent, OsStr::new(retired), mount, configuration)
					.await
			}
			None => Err(SubmoduleError::RecoveryRequired(
				"mounted deinit intent has no checkout retirement location".to_owned(),
			)),
		}
	}

	fn pinned_module(
		&self,
		intent: &DeinitIntent,
		retained: Option<Dir>,
	) -> Result<Option<Dir>, SubmoduleError> {
		let Some(expected) = intent.module_identity else {
			if intent.module_transition.is_some() || intent.mount_identity.is_some() || retained.is_some()
			{
				return Err(SubmoduleError::RecoveryRequired(
					"deinit intent does not identify its module repository".to_owned(),
				));
			}
			return Ok(None);
		};
		let expected: EntryIdentity = expected.into();
		let relative = Path::new("modules").join(&intent.name);
		let current = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(intent.name.clone()))?;
		let current_identity = directory_identity(&current).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(&relative),
			source,
		})?;
		if current_identity != expected {
			return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
		}
		if let Some(retained) = retained {
			if directory_identity(&retained).map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(&relative),
				source,
			})? != expected
			{
				return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
			}
			Ok(Some(retained))
		} else {
			Ok(Some(current))
		}
	}

	fn ensure_recorded_module_identity(
		&self,
		intent: &DeinitIntent,
		relative: &Path,
		module: &Dir,
	) -> Result<(), SubmoduleError> {
		let expected: EntryIdentity = intent
			.module_identity
			.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"deinit intent does not identify its module repository".to_owned(),
				)
			})?
			.into();
		if directory_identity(module).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(relative),
			source,
		})? != expected
		{
			return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
		}
		self.ensure_deinit_module_identity(relative, expected)
	}

	fn ensure_recorded_planned_module_identity(
		&self,
		name: &str,
		relative: &Path,
		module: &Dir,
		expected: EntryIdentity,
	) -> Result<(), SubmoduleError> {
		if directory_identity(module).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(relative),
			source,
		})? != expected
		{
			return Err(SubmoduleError::InvalidRepository(name.to_owned()));
		}
		self.ensure_deinit_module_identity(relative, expected)
	}

	fn ensure_deinit_module_identity(
		&self,
		relative: &Path,
		expected: EntryIdentity,
	) -> Result<(), SubmoduleError> {
		let current = self
			.open_git_subdir_nofollow(relative)
			.map_err(|_| SubmoduleError::InvalidRepository(relative.display().to_string()))?;
		if directory_identity(&current).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(relative),
			source,
		})? != expected
		{
			return Err(SubmoduleError::InvalidRepository(
				relative.display().to_string(),
			));
		}
		Ok(())
	}

	async fn ensure_displaced_owned<R: MarkerTargetResolver>(
		&self,
		intent: &DeinitIntent,
		parent: &Dir,
		displaced: &OsStr,
		expected: EntryIdentity,
		_configuration: &R,
	) -> Result<(), SubmoduleError> {
		let directory = parent
			.open_dir_nofollow(displaced)
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&intent.path),
				source,
			})?;
		if directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().join(&intent.path),
			source,
		})? != expected
		{
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		let expected_marker = intent.mount_marker.as_ref().ok_or_else(|| {
			SubmoduleError::RecoveryRequired("mounted deinit intent has no exact mount marker".to_owned())
		})?;
		let marker = self.read_mount_marker(
			&SubmoduleDeclaration {
				name: intent.name.clone(),
				path: intent.path.clone(),
				url: None,
				branch: None,
				update: None,
				shallow: None,
			},
			&directory,
		)?;
		if &marker != expected_marker {
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		if identity_if_present(parent, displaced)? != Some(expected) {
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		Ok(())
	}

	pub(crate) async fn capture_owned_mount<R: MarkerTargetResolver>(
		&self,
		declaration: &SubmoduleDeclaration,
		parent: &Dir,
		target: &OsStr,
		configuration: &R,
	) -> Result<(Dir, EntryIdentity, DeinitMountMarker), SubmoduleError> {
		let directory = parent
			.open_dir_nofollow(target)
			.map_err(|_| SubmoduleError::ForeignMount(declaration.path.clone()))?;
		let identity = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().join(&declaration.path),
			source,
		})?;
		let marker = self
			.ensure_mount_marker_owned(declaration, &directory, configuration)
			.await?;
		if entry_identity(parent, target).map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().join(&declaration.path),
			source,
		})? != identity
		{
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		Ok((directory, identity, marker))
	}

	pub(crate) async fn revalidate_owned_mount<R: MarkerTargetResolver>(
		&self,
		declaration: &SubmoduleDeclaration,
		parent: &Dir,
		target: &OsStr,
		expected: EntryIdentity,
		expected_marker: &DeinitMountMarker,
		configuration: &R,
	) -> Result<EntryIdentity, SubmoduleError> {
		let (_, current, marker) = self
			.capture_owned_mount(declaration, parent, target, configuration)
			.await?;
		if current != expected || &marker != expected_marker {
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		Ok(current)
	}

	async fn ensure_mount_marker_owned<R: MarkerTargetResolver>(
		&self,
		declaration: &SubmoduleDeclaration,
		directory: &Dir,
		configuration: &R,
	) -> Result<DeinitMountMarker, SubmoduleError> {
		let before = self.read_mount_marker(declaration, directory)?;
		let target = std::str::from_utf8(&before.bytes)
			.ok()
			.and_then(crate::context::parse_marker_target)
			.ok_or_else(|| SubmoduleError::ForeignMount(declaration.path.clone()))?;
		if !self
			.marker_targets_expected(declaration, target, configuration)
			.await?
		{
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		let after = self.read_mount_marker(declaration, directory)?;
		if after != before {
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		Ok(before)
	}

	fn read_mount_marker(
		&self,
		declaration: &SubmoduleDeclaration,
		directory: &Dir,
	) -> Result<DeinitMountMarker, SubmoduleError> {
		let display = self.worktree_root().join(&declaration.path).join(".git");
		let mut options = OpenOptions::new();
		options.read(true).follow(FollowSymlinks::No);
		let mut file = directory
			.open_with(".git", &options)
			.map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?;
		let metadata = file.metadata().map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		if !metadata.is_file() || metadata.file_type().is_symlink() {
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		let mut bytes = Vec::new();
		file
			.read_to_end(&mut bytes)
			.map_err(|source| SubmoduleError::Io {
				path: display,
				source,
			})?;
		Ok(DeinitMountMarker {
			identity: identity.into(),
			bytes,
		})
	}

	async fn ensure_displaced_clean<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		intent: &DeinitIntent,
		parent: &Dir,
		displaced: &OsStr,
		module: &Dir,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		let mount = parent
			.open_dir_nofollow(displaced)
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&intent.path),
				source,
			})?;
		let declaration = SubmoduleDeclaration {
			name: intent.name.clone(),
			path: intent.path.clone(),
			url: None,
			update: None,
			branch: None,
			shallow: None,
		};
		self
			.ensure_module_repository_valid::<H, C>(&declaration, module, configuration)
			.await?;
		self
			.ensure_checkout_clean_with_module::<H, C>(
				&declaration,
				mount,
				module,
				ObjectId::<H>::from_hex(&intent.recorded).map_err(|_| {
					SubmoduleError::RecoveryRequired(
						"deinit intent records an invalid gitlink object id".to_owned(),
					)
				})?,
				configuration,
			)
			.await
	}

	async fn ensure_checkout_clean_with_module<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		mount: Dir,
		module: &Dir,
		recorded: ObjectId<H>,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		let relative = Path::new("modules").join(&declaration.name);
		let module_path = self.layout.git_dir.join(&relative);
		let config_dir = module.try_clone().map_err(|source| SubmoduleError::Io {
			path: module_path.clone(),
			source,
		})?;
		let config = configuration
			.load_module_config(config_dir, &module_path)
			.await?;
		let mount_path = self.worktree_root().join(&declaration.path);
		let excludes = configuration
			.load_module_excludes_at(
				&config,
				mount.try_clone().map_err(|source| SubmoduleError::Io {
					path: mount_path.clone(),
					source,
				})?,
				&mount_path,
			)
			.await?;
		let files =
			LocalFileStore::from_dir(module.try_clone().map_err(|source| SubmoduleError::Io {
				path: module_path.clone(),
				source,
			})?);
		let mut repository = Repository::<_, H>::new(ObjectStore::new(files));
		repository.set_effective_config(config);
		if repository.refs().resolve_head().await? != Some(recorded) {
			return Err(SubmoduleError::LocalModifications(declaration.path.clone()));
		}
		let worktree = WorkTree::new_located(
			repository,
			CapWorkDir::from_dir(mount),
			module_path,
			mount_path,
		);
		let status = worktree.status(excludes.as_deref()).await?;
		let diverged = worktree.diverged_tracked_content_paths().await?;
		if !status.changed.is_empty() || !status.untracked.is_empty() || !diverged.is_empty() {
			return Err(SubmoduleError::LocalModifications(declaration.path.clone()));
		}
		Ok(())
	}

	pub(crate) async fn ensure_module_repository_valid<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		module: &Dir,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		let relative = Path::new("modules").join(&declaration.name);
		let module_path = self.layout.git_dir.join(&relative);
		let config_dir = module.try_clone().map_err(|source| SubmoduleError::Io {
			path: module_path.clone(),
			source,
		})?;
		if configuration
			.module_hash_kind(config_dir, &module_path)
			.await?
			!= crate::object_id::kind::<H>()
		{
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
		}
		Ok(())
	}

	pub(crate) fn mount_is_attached(
		&self,
		declaration: &SubmoduleDeclaration,
	) -> Result<bool, SubmoduleError> {
		let Some(directory) = self.existing_mount_directory_nofollow(&declaration.path)? else {
			return Ok(false);
		};
		match directory.symlink_metadata(".git") {
			Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
			Ok(_) => Err(SubmoduleError::ForeignMount(declaration.path.clone())),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
			Err(source) => Err(SubmoduleError::Io {
				path: self.worktree_root().join(&declaration.path).join(".git"),
				source,
			}),
		}
	}

	fn validate_deinit_intent(
		&self,
		intent: &DeinitIntent,
		entry: &Planned,
	) -> Result<(), SubmoduleError> {
		validate_deinit_intent_shape(intent)?;
		if intent.name != entry.declaration.name
			|| intent.path != entry.declaration.path
			|| intent.recorded != entry.recorded
			|| intent.core_worktree != entry.core_worktree
		{
			return Err(SubmoduleError::RecoveryRequired(
				"deinit intent does not match the selected gitlink and declaration".to_owned(),
			));
		}
		Ok(())
	}

	fn ensure_current_public_mount_empty(&self, intent: &DeinitIntent) -> Result<(), SubmoduleError> {
		let (parent, target, _) = self.mount_parent(&intent.path)?;
		self.ensure_empty_identity(
			&parent,
			&target,
			intent.public_identity.into(),
			&intent.path,
		)
	}

	async fn ensure_current_public_mount_owned<R: MarkerTargetResolver>(
		&self,
		intent: &DeinitIntent,
		expected: EntryIdentity,
		configuration: &R,
	) -> Result<(), SubmoduleError> {
		let (parent, target, _) = self.mount_parent(&intent.path)?;
		let declaration = SubmoduleDeclaration {
			name: intent.name.clone(),
			path: intent.path.clone(),
			url: None,
			branch: None,
			update: None,
			shallow: None,
		};
		let expected_marker = intent.mount_marker.as_ref().ok_or_else(|| {
			SubmoduleError::RecoveryRequired("mounted deinit intent has no exact mount marker".to_owned())
		})?;
		let (_, current, marker) = self
			.capture_owned_mount(&declaration, &parent, &target, configuration)
			.await?;
		if current != expected || &marker != expected_marker {
			return Err(SubmoduleError::ForeignMount(intent.path.clone()));
		}
		Ok(())
	}

	pub(crate) fn mount_parent(
		&self,
		path: &str,
	) -> Result<(Dir, OsString, PathBuf), SubmoduleError> {
		let path = Path::new(path);
		let target = path
			.file_name()
			.ok_or_else(|| SubmoduleError::UnsafePath(path.display().to_string()))?
			.to_owned();
		let parent_path = path.parent().unwrap_or(Path::new(""));
		let parent = self.open_work_subdir_nofollow(parent_path)?;
		Ok((parent, target, self.worktree_root().join(parent_path)))
	}

	fn open_work_subdir_nofollow(&self, relative: &Path) -> Result<Dir, SubmoduleError> {
		let mut current = self.clone_dir(&self.work, self.worktree_root())?;
		let mut traversed = PathBuf::new();
		for component in relative.components() {
			let Component::Normal(component) = component else {
				return Err(SubmoduleError::UnsafePath(relative.display().to_string()));
			};
			traversed.push(component);
			current = current
				.open_dir_nofollow(component)
				.map_err(|source| SubmoduleError::Io {
					path: self.worktree_root().join(&traversed),
					source,
				})?;
		}
		Ok(current)
	}

	fn create_private_empty_directory(
		&self,
		parent: &Dir,
		purpose: &str,
		display: &Path,
	) -> Result<OsString, SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let name = self.private_absent_name(parent, purpose, display)?;
			match parent.create_dir(&name) {
				Ok(()) => {
					sync_directory(parent, display)?;
					return Ok(name);
				}
				Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: display.join(&name),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::Io {
			path: display.to_owned(),
			source: std::io::Error::new(
				std::io::ErrorKind::AlreadyExists,
				"could not reserve a private deinit directory",
			),
		})
	}

	fn private_absent_name(
		&self,
		parent: &Dir,
		purpose: &str,
		display: &Path,
	) -> Result<OsString, SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let sequence = PRIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
			let name = OsString::from(format!(
				".gitana-submodule-deinit-{purpose}.{}.{}",
				std::process::id(),
				sequence
			));
			match parent.symlink_metadata(&name) {
				Ok(_) => {}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: display.join(&name),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::Io {
			path: display.to_owned(),
			source: std::io::Error::new(
				std::io::ErrorKind::AlreadyExists,
				"could not reserve a private deinit name",
			),
		})
	}

	fn private_retirement_name(
		&self,
		parent: &Dir,
		module: &Dir,
		parent_display: &Path,
		module_display: &Path,
	) -> Result<OsString, SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let name = self.private_absent_name(parent, "retired", parent_display)?;
			match module.symlink_metadata(&name) {
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
				Ok(_) => {}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: module_display.join(name),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::RecoveryRequired(
			"could not reserve a private deinit retirement name".to_owned(),
		))
	}

	fn ensure_empty_identity(
		&self,
		parent: &Dir,
		target: &OsStr,
		expected: EntryIdentity,
		path: &str,
	) -> Result<(), SubmoduleError> {
		if entry_identity(parent, target).map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().join(path),
			source,
		})? != expected
		{
			return Err(SubmoduleError::ForeignMount(path.to_owned()));
		}
		let directory = parent
			.open_dir_nofollow(target)
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(path),
				source,
			})?;
		self.validate_opened_empty_identity(parent, target, &directory, expected, path)
	}

	fn validate_opened_empty_identity(
		&self,
		parent: &Dir,
		target: &OsStr,
		directory: &Dir,
		expected: EntryIdentity,
		path: &str,
	) -> Result<(), SubmoduleError> {
		let display = self.worktree_root().join(path);
		if directory_identity(directory).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})? != expected
		{
			return Err(SubmoduleError::ForeignMount(path.to_owned()));
		}
		if directory
			.entries()
			.map_err(|source| SubmoduleError::Io {
				path: display.clone(),
				source,
			})?
			.next()
			.is_some()
		{
			return Err(SubmoduleError::ForeignMount(path.to_owned()));
		}
		if entry_identity(parent, target).map_err(|source| SubmoduleError::Io {
			path: display,
			source,
		})? != expected
		{
			return Err(SubmoduleError::ForeignMount(path.to_owned()));
		}
		Ok(())
	}

	pub(crate) fn ensure_no_deinit_recovery(&self) -> Result<(), SubmoduleError> {
		ensure_no_deinit_recovery_at(&self.git, &self.layout.git_dir)
	}

	pub(crate) fn ensure_no_repository_deinit_recovery(&self) -> Result<(), SubmoduleError> {
		for (git_dir, git) in deinit_recovery_git_dirs(&self.common, &self.git, &self.layout)? {
			ensure_no_deinit_recovery_at(&git, &git_dir)?;
		}
		Ok(())
	}

	fn ensure_no_other_deinit_recovery(&self) -> Result<(), SubmoduleError> {
		let current = directory_identity(&self.git).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.clone(),
			source,
		})?;
		for (git_dir, git) in deinit_recovery_git_dirs(&self.common, &self.git, &self.layout)? {
			let identity = directory_identity(&git).map_err(|source| SubmoduleError::Io {
				path: git_dir.clone(),
				source,
			})?;
			ensure_no_set_url_recovery_at(&git, &git_dir)?;
			if identity != current {
				ensure_no_deinit_intent_at(&git, &git_dir)?;
			}
		}
		Ok(())
	}

	pub(crate) fn ensure_no_update_recovery(&self) -> Result<(), SubmoduleError> {
		match self.git.symlink_metadata(UPDATE_CONTROL_DIR) {
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
				Err(SubmoduleError::RecoveryRequired(
					"pending submodule update recovery must be completed before deinit".to_owned(),
				))
			}
			Ok(_) => Err(SubmoduleError::RecoveryRequired(
				"submodule update control path is not a directory".to_owned(),
			)),
			Err(source) => Err(SubmoduleError::Io {
				path: self.layout.git_dir.join(UPDATE_CONTROL_DIR),
				source,
			}),
		}
	}

	fn read_deinit_intent(&self) -> Result<Option<(DeinitIntent, EntryIdentity)>, SubmoduleError> {
		read_deinit_intent_at(&self.git, &self.layout.git_dir)
	}

	fn ensure_active_deinit_intent(&self, expected: EntryIdentity) -> Result<(), SubmoduleError> {
		ensure_active_deinit_intent_at(&self.git, &self.layout.git_dir, expected)
	}

	fn publish_deinit_intent(&self, intent: &DeinitIntent) -> Result<EntryIdentity, SubmoduleError> {
		if self.read_deinit_intent()?.is_some() {
			return Err(SubmoduleError::RecoveryRequired(
				"another deinit intent is already pending".to_owned(),
			));
		}
		self.prepare_empty_control_dir()?;
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		let bytes = serde_json::to_vec(intent).map_err(|error| {
			SubmoduleError::RecoveryRequired(format!("serializing deinit intent: {error}"))
		})?;
		let mut options = OpenOptions::new();
		options
			.write(true)
			.create_new(true)
			.follow(FollowSymlinks::No);
		let mut file = control
			.open_with(INTENT_LOCK_NAME, &options)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_LOCK_NAME),
				source,
			})?;
		let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_LOCK_NAME),
			source,
		})?;
		file
			.write_all(&bytes)
			.and_then(|()| file.sync_all())
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_LOCK_NAME),
				source,
			})?;
		drop(file);
		rename_noreplace_if_identity(
			&control,
			OsStr::new(INTENT_LOCK_NAME),
			identity,
			&control,
			OsStr::new(INTENT_NAME),
		)
		.map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_NAME),
			source,
		})?;
		sync_directory(&control, &self.layout.git_dir.join(CONTROL_DIR))?;
		Ok(identity)
	}

	fn advance_deinit_intent(
		&self,
		intent: &mut DeinitIntent,
		expected: EntryIdentity,
		phase: DeinitPhase,
	) -> Result<EntryIdentity, SubmoduleError> {
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		#[cfg(windows)]
		self.retire_previous_intent(&control, intent)?;
		intent.phase = phase;
		let bytes = serde_json::to_vec(intent).map_err(|error| {
			SubmoduleError::RecoveryRequired(format!("serializing deinit intent: {error}"))
		})?;
		let prepared = self.private_intent_name(&control)?;
		let mut options = OpenOptions::new();
		options
			.write(true)
			.create_new(true)
			.follow(FollowSymlinks::No);
		let mut file = control
			.open_with(&prepared, &options)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(&prepared),
				source,
			})?;
		let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR).join(&prepared),
			source,
		})?;
		file
			.write_all(&bytes)
			.and_then(|()| file.sync_all())
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(&prepared),
				source,
			})?;
		drop(file);
		#[cfg(not(windows))]
		{
			replace_if_identities(
				&control,
				&prepared,
				identity,
				OsStr::new(INTENT_NAME),
				expected,
			)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_NAME),
				source,
			})?;
			sync_directory(&control, &self.layout.git_dir.join(CONTROL_DIR))?;
		}
		#[cfg(windows)]
		{
			rename_noreplace_if_identity(
				&control,
				OsStr::new(INTENT_NAME),
				expected,
				&control,
				OsStr::new(INTENT_PREVIOUS_NAME),
			)
			.map_err(|source| SubmoduleError::Io {
				path: self
					.layout
					.git_dir
					.join(CONTROL_DIR)
					.join(INTENT_PREVIOUS_NAME),
				source,
			})?;
			sync_directory(&control, &self.layout.git_dir.join(CONTROL_DIR))?;
			rename_noreplace_if_identity(
				&control,
				&prepared,
				identity,
				&control,
				OsStr::new(INTENT_NAME),
			)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_NAME),
				source,
			})?;
			sync_directory(&control, &self.layout.git_dir.join(CONTROL_DIR))?;
			remove_file_if_identity(&control, OsStr::new(INTENT_PREVIOUS_NAME), expected).map_err(
				|source| SubmoduleError::Io {
					path: self
						.layout
						.git_dir
						.join(CONTROL_DIR)
						.join(INTENT_PREVIOUS_NAME),
					source,
				},
			)?;
			sync_directory(&control, &self.layout.git_dir.join(CONTROL_DIR))?;
		}
		Ok(identity)
	}

	#[cfg(windows)]
	fn retire_previous_intent(
		&self,
		control: &Dir,
		current: &DeinitIntent,
	) -> Result<(), SubmoduleError> {
		let display = self
			.layout
			.git_dir
			.join(CONTROL_DIR)
			.join(INTENT_PREVIOUS_NAME);
		let Some((previous, identity)) =
			read_named_intent_file(control, OsStr::new(INTENT_PREVIOUS_NAME), &display)?
		else {
			return Ok(());
		};
		if !same_deinit_transaction(&previous, current) || previous.phase > current.phase {
			return Err(SubmoduleError::RecoveryRequired(
				"deinit intent predecessor does not match the active transaction".to_owned(),
			));
		}
		remove_file_if_identity(control, OsStr::new(INTENT_PREVIOUS_NAME), identity).map_err(
			|source| SubmoduleError::Io {
				path: display,
				source,
			},
		)?;
		sync_directory(control, &self.layout.git_dir.join(CONTROL_DIR))
	}

	fn private_intent_name(&self, control: &Dir) -> Result<OsString, SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let sequence = PRIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
			let name = OsString::from(format!(
				".gitana-submodule-deinit-intent.{}.{}",
				std::process::id(),
				sequence
			));
			match control.symlink_metadata(&name) {
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
				Ok(_) => {}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: self.layout.git_dir.join(CONTROL_DIR).join(name),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::RecoveryRequired(
			"could not reserve a private deinit journal update".to_owned(),
		))
	}

	fn prepare_empty_control_dir(&self) -> Result<(), SubmoduleError> {
		self.retire_unpublished_deinit_control()?;
		self
			.git
			.create_dir(CONTROL_DIR)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		sync_directory(&self.git, &self.layout.git_dir)?;
		Ok(())
	}

	fn retire_unpublished_deinit_control(&self) -> Result<(), SubmoduleError> {
		match self.git.symlink_metadata(CONTROL_DIR) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
				let control = self
					.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
					.map_err(|source| SubmoduleError::Io {
						path: self.layout.git_dir.join(CONTROL_DIR),
						source,
					})?;
				if read_intent_file(
					&control,
					&self.layout.git_dir.join(CONTROL_DIR).join(INTENT_NAME),
				)?
				.is_some()
				{
					return Err(SubmoduleError::RecoveryRequired(
						"another deinit intent is already pending".to_owned(),
					));
				}
				let identity = directory_identity(&control).map_err(|source| SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				})?;
				self.retire_deinit_control(identity)?;
			}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"submodule deinit control path is not a directory".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				});
			}
		}
		Ok(())
	}

	fn clear_deinit_intent(&self, expected: EntryIdentity) -> Result<(), SubmoduleError> {
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		if entry_identity(&control, OsStr::new(INTENT_NAME)).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR).join(INTENT_NAME),
			source,
		})? != expected
		{
			return Err(SubmoduleError::RecoveryRequired(
				"deinit intent changed before journal retirement".to_owned(),
			));
		}
		let identity = directory_identity(&control).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR),
			source,
		})?;
		self.retire_deinit_control(identity)
	}

	fn retire_deinit_control(&self, expected: EntryIdentity) -> Result<(), SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let sequence = RETIRE_COUNTER.fetch_add(1, Ordering::Relaxed);
			let retired = format!(
				".gitana-submodule-deinit-retired.{}.{}",
				std::process::id(),
				sequence
			);
			match rename_noreplace_if_identity(
				&self.git,
				OsStr::new(CONTROL_DIR),
				expected,
				&self.git,
				OsStr::new(&retired),
			) {
				Ok(()) => {
					sync_directory(&self.git, &self.layout.git_dir)?;
					return Ok(());
				}
				Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: self.layout.git_dir.join(CONTROL_DIR),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::RecoveryRequired(
			"could not retire the completed deinit journal".to_owned(),
		))
	}
}

fn deinit_query(selection: &DeinitSelection) -> Result<crate::SubmoduleQuery, SubmoduleError> {
	match selection {
		DeinitSelection::All => Ok(crate::SubmoduleQuery::all()),
		DeinitSelection::Paths(pathspecs) if pathspecs.is_empty() => {
			Err(SubmoduleError::EmptyDeinitSelection)
		}
		DeinitSelection::Paths(pathspecs) => Ok(crate::SubmoduleQuery::paths(pathspecs.clone())),
	}
}

fn ensure_distinct_deinit_config_targets(
	targets: &[(String, crate::DeinitConfigTarget)],
) -> Result<(), SubmoduleError> {
	for (index, (left_name, left)) in targets.iter().enumerate() {
		let Some(left_identity) = left.target() else {
			continue;
		};
		for (right_name, right) in &targets[index + 1..] {
			if right.target() == Some(left_identity) {
				return Err(SubmoduleError::Configuration(format!(
					"deinit config targets for {left_name} and {right_name} resolve to the same file"
				)));
			}
		}
	}
	Ok(())
}

fn validate_deinit_intent_shape(intent: &DeinitIntent) -> Result<(), SubmoduleError> {
	if intent.version != INTENT_VERSION {
		return Err(SubmoduleError::RecoveryRequired(format!(
			"unsupported deinit intent version {}",
			intent.version
		)));
	}
	validate_name(&intent.name)?;
	validate_path(&intent.path)?;
	let path = Path::new(&intent.path);
	let expected_parent = path.parent().unwrap_or(Path::new(""));
	let expected_target = path
		.file_name()
		.ok_or_else(|| SubmoduleError::UnsafePath(intent.path.clone()))?;
	if intent.parent != path_string(expected_parent)? || intent.target != os_string(expected_target)?
	{
		return Err(SubmoduleError::RecoveryRequired(
			"deinit intent mount namespace does not match its selected path".to_owned(),
		));
	}
	if intent.phase < DeinitPhase::Retired && intent.retirement_location.is_some() {
		return Err(SubmoduleError::RecoveryRequired(
			"deinit intent records a checkout retirement before its retirement phase".to_owned(),
		));
	}
	match (&intent.module_transition, &intent.module_publication) {
		(None, None) => {}
		(None, Some(_)) => {
			return Err(SubmoduleError::RecoveryRequired(
				"deinit intent records a module config publication without a transition".to_owned(),
			));
		}
		(Some(transition), publication) => validate_config_publication(
			transition,
			publication.as_ref(),
			intent.phase,
			DeinitPhase::ModuleConfigReserved,
			DeinitPhase::ModuleConfigPrepared,
			"module",
		)?,
	}
	validate_config_publication(
		&intent.super_transition,
		intent.super_publication.as_ref(),
		intent.phase,
		DeinitPhase::SuperConfigReserved,
		DeinitPhase::SuperConfigPrepared,
		"superproject",
	)?;
	if intent.mount_identity.is_none() && intent.mount_marker.is_some() {
		return Err(SubmoduleError::RecoveryRequired(
			"unmounted deinit intent records a mount marker".to_owned(),
		));
	}
	match (
		intent.mount_identity,
		intent.prepared_identity,
		intent.prepared.as_deref(),
		intent.displaced.as_deref(),
		intent.retired.as_deref(),
	) {
		(None, None, None, None, None)
			if intent.rollback.is_none() && intent.retirement_location.is_none() =>
		{
			Ok(())
		}
		(Some(_), Some(_), Some(prepared), Some(displaced), Some(retired))
			if private_component(prepared, &["prepared"])
				&& private_component(displaced, &["prepared", "displaced"])
				&& private_component(retired, &["retired"])
				&& intent.module_identity.is_some() =>
		{
			#[cfg(windows)]
			if displaced == prepared {
				return Err(SubmoduleError::RecoveryRequired(
					"Windows deinit intent aliases its prepared and displaced directories".to_owned(),
				));
			}
			let two_step = displaced != prepared
				&& intent
					.rollback
					.as_deref()
					.is_some_and(|rollback| private_component(rollback, &["rollback"]));
			#[cfg(not(windows))]
			let legacy_exchange = displaced == prepared && intent.rollback.is_none();
			#[cfg(windows)]
			let legacy_exchange = false;
			if !two_step && !legacy_exchange {
				return Err(SubmoduleError::RecoveryRequired(
					"deinit intent has no safe namespace transition names".to_owned(),
				));
			}
			if intent.phase >= DeinitPhase::Retired && intent.retirement_location.is_none() {
				return Err(SubmoduleError::RecoveryRequired(
					"retired deinit intent does not record the checkout location".to_owned(),
				));
			}
			Ok(())
		}
		_ => Err(SubmoduleError::RecoveryRequired(
			"deinit intent contains an incomplete mount transaction".to_owned(),
		)),
	}
}

fn validate_config_publication(
	transition: &DeinitConfigTransition,
	publication: Option<&DeinitConfigPublication>,
	phase: DeinitPhase,
	reserved_phase: DeinitPhase,
	prepared_phase: DeinitPhase,
	owner: &str,
) -> Result<(), SubmoduleError> {
	if !transition.changes() {
		if publication.is_some() {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"unchanged {owner} config transition records a publication"
			)));
		}
		return Ok(());
	}
	match (phase >= reserved_phase, publication) {
		(false, None) => Ok(()),
		(true, Some(publication)) if private_config_component(&publication.name) => Ok(()),
		(false, Some(_)) => Err(SubmoduleError::RecoveryRequired(format!(
			"{owner} config reservation precedes its journal phase"
		))),
		(true, None) => Err(SubmoduleError::RecoveryRequired(format!(
			"reserved {owner} config transition has no recorded publication"
		))),
		(true, Some(_)) => Err(SubmoduleError::RecoveryRequired(format!(
			"{owner} config publication name is unsafe"
		))),
	}?;
	if phase >= prepared_phase && publication.is_none() {
		return Err(SubmoduleError::RecoveryRequired(format!(
			"prepared {owner} config transition has no recorded publication"
		)));
	}
	Ok(())
}

fn read_deinit_intent_at(
	git: &Dir,
	git_dir: &Path,
) -> Result<Option<(DeinitIntent, EntryIdentity)>, SubmoduleError> {
	let control = match open_git_subdir_nofollow(git, Path::new(CONTROL_DIR)) {
		Ok(control) => control,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: git_dir.join(CONTROL_DIR),
				source,
			});
		}
	};
	read_intent_file(&control, &git_dir.join(CONTROL_DIR).join(INTENT_NAME))
}

fn ensure_active_deinit_intent_at(
	git: &Dir,
	git_dir: &Path,
	expected: EntryIdentity,
) -> Result<(), SubmoduleError> {
	let control =
		open_git_subdir_nofollow(git, Path::new(CONTROL_DIR)).map_err(|source| SubmoduleError::Io {
			path: git_dir.join(CONTROL_DIR),
			source,
		})?;
	ensure_named_intent_identity(
		&control,
		OsStr::new(INTENT_NAME),
		expected,
		"active deinit intent changed before recovery",
	)
}

fn open_git_subdir_nofollow(git: &Dir, relative: &Path) -> std::io::Result<Dir> {
	let mut current = git.try_clone()?;
	for component in relative.components() {
		let Component::Normal(component) = component else {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"unsafe module git directory",
			));
		};
		let metadata = current.symlink_metadata(component)?;
		if metadata.file_type().is_symlink() || !metadata.is_dir() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"module git directory component is not a directory",
			));
		}
		current = current.open_dir_nofollow(component)?;
	}
	Ok(current)
}

fn read_intent_file(
	control: &Dir,
	display: &Path,
) -> Result<Option<(DeinitIntent, EntryIdentity)>, SubmoduleError> {
	#[cfg(windows)]
	return read_windows_intent_file(control, display);
	#[cfg(not(windows))]
	read_named_intent_file(control, OsStr::new(INTENT_NAME), display)
}

#[cfg(any(windows, test))]
fn read_windows_intent_file(
	control: &Dir,
	display: &Path,
) -> Result<Option<(DeinitIntent, EntryIdentity)>, SubmoduleError> {
	let current = read_named_intent_file(control, OsStr::new(INTENT_NAME), display)?;
	let previous_display = display.with_file_name(INTENT_PREVIOUS_NAME);
	let previous =
		read_named_intent_file(control, OsStr::new(INTENT_PREVIOUS_NAME), &previous_display)?;
	match (current, previous) {
		(Some(current), Some(previous)) => {
			ensure_named_intent_identity(
				control,
				OsStr::new(INTENT_NAME),
				current.1,
				"active deinit intent changed while it was being read",
			)?;
			ensure_named_intent_identity(
				control,
				OsStr::new(INTENT_PREVIOUS_NAME),
				previous.1,
				"deinit intent predecessor changed while it was being read",
			)?;
			if !same_deinit_transaction(&previous.0, &current.0) || previous.0.phase > current.0.phase {
				return Err(SubmoduleError::RecoveryRequired(
					"deinit intent predecessor does not match the active transaction".to_owned(),
				));
			}
			sync_directory(control, display.parent().unwrap_or(Path::new("")))?;
			Ok(Some(current))
		}
		(Some(current), None) => {
			ensure_named_intent_identity(
				control,
				OsStr::new(INTENT_NAME),
				current.1,
				"active deinit intent changed while it was being read",
			)?;
			if identity_if_present(control, OsStr::new(INTENT_PREVIOUS_NAME))?.is_some() {
				return Err(SubmoduleError::RecoveryRequired(
					"deinit intent predecessor appeared while the journal was being read".to_owned(),
				));
			}
			Ok(Some(current))
		}
		(None, Some((intent, identity))) => Ok(Some(promote_windows_intent_predecessor(
			control, display, intent, identity,
		)?)),
		(None, None) => Ok(None),
	}
}

#[cfg(any(windows, test))]
fn promote_windows_intent_predecessor(
	control: &Dir,
	display: &Path,
	intent: DeinitIntent,
	identity: EntryIdentity,
) -> Result<(DeinitIntent, EntryIdentity), SubmoduleError> {
	match rename_noreplace_if_identity(
		control,
		OsStr::new(INTENT_PREVIOUS_NAME),
		identity,
		control,
		OsStr::new(INTENT_NAME),
	) {
		Ok(()) => finish_windows_intent_predecessor_promotion(control, display, identity)
			.map(|()| (intent, identity)),
		Err(source)
			if matches!(
				source.kind(),
				std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
			) =>
		{
			let current = read_named_intent_file(control, OsStr::new(INTENT_NAME), display)?;
			if identity_if_present(control, OsStr::new(INTENT_PREVIOUS_NAME))?.is_none()
				&& let Some((current_intent, current_identity)) = current
				&& current_identity == identity
				&& current_intent == intent
			{
				finish_windows_intent_predecessor_promotion(control, display, identity)?;
				return Ok((current_intent, current_identity));
			}
			Err(SubmoduleError::Io {
				path: display.to_owned(),
				source,
			})
		}
		Err(source) => Err(SubmoduleError::Io {
			path: display.to_owned(),
			source,
		}),
	}
}

#[cfg(any(windows, test))]
fn finish_windows_intent_predecessor_promotion(
	control: &Dir,
	display: &Path,
	identity: EntryIdentity,
) -> Result<(), SubmoduleError> {
	sync_directory(control, display.parent().unwrap_or(Path::new("")))?;
	ensure_named_intent_identity(
		control,
		OsStr::new(INTENT_NAME),
		identity,
		"restored deinit intent changed before recovery",
	)?;
	if identity_if_present(control, OsStr::new(INTENT_PREVIOUS_NAME))?.is_some() {
		return Err(SubmoduleError::RecoveryRequired(
			"deinit intent predecessor appeared while its promotion was being recovered".to_owned(),
		));
	}
	Ok(())
}

fn read_named_intent_file(
	control: &Dir,
	name: &OsStr,
	display: &Path,
) -> Result<Option<(DeinitIntent, EntryIdentity)>, SubmoduleError> {
	match control.symlink_metadata(name) {
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"deinit intent is not a regular file".to_owned(),
			));
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: display.to_owned(),
				source,
			});
		}
	}
	let mut options = OpenOptions::new();
	options.read(true).follow(FollowSymlinks::No);
	let mut file = control
		.open_with(name, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?;
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: display.to_owned(),
		source,
	})?;
	let mut bytes = Vec::new();
	file
		.read_to_end(&mut bytes)
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?;
	let intent = serde_json::from_slice(&bytes)
		.map_err(|error| SubmoduleError::RecoveryRequired(format!("invalid deinit intent: {error}")))?;
	ensure_named_intent_identity(
		control,
		name,
		identity,
		"deinit intent changed while it was being read",
	)?;
	Ok(Some((intent, identity)))
}

fn ensure_named_intent_identity(
	control: &Dir,
	name: &OsStr,
	expected: EntryIdentity,
	changed: &str,
) -> Result<(), SubmoduleError> {
	if identity_if_present(control, name)? != Some(expected) {
		return Err(SubmoduleError::RecoveryRequired(changed.to_owned()));
	}
	Ok(())
}

#[cfg(any(windows, test))]
fn same_deinit_transaction(left: &DeinitIntent, right: &DeinitIntent) -> bool {
	left.version == right.version
		&& left.name == right.name
		&& left.path == right.path
		&& left.recorded == right.recorded
		&& left.force == right.force
		&& left.core_worktree == right.core_worktree
		&& left.module_transition == right.module_transition
		&& preserves_optional_proof(&left.module_publication, &right.module_publication)
		&& left.super_transition == right.super_transition
		&& preserves_optional_proof(&left.super_publication, &right.super_publication)
		&& left.parent == right.parent
		&& left.target == right.target
		&& left.prepared == right.prepared
		&& left.displaced == right.displaced
		&& left.mount_identity == right.mount_identity
		&& preserves_optional_proof(&left.mount_marker, &right.mount_marker)
		&& left.prepared_identity == right.prepared_identity
		&& left.public_identity == right.public_identity
		&& left.module_identity == right.module_identity
		&& left.retired == right.retired
		&& left.rollback == right.rollback
		&& preserves_optional_proof(&left.retirement_location, &right.retirement_location)
}

#[cfg(any(windows, test))]
fn preserves_optional_proof<T: PartialEq>(previous: &Option<T>, current: &Option<T>) -> bool {
	previous
		.as_ref()
		.is_none_or(|expected| current.as_ref() == Some(expected))
}

fn identity_if_present(
	directory: &Dir,
	name: &OsStr,
) -> Result<Option<EntryIdentity>, SubmoduleError> {
	match entry_identity(directory, name) {
		Ok(identity) => Ok(Some(identity)),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(source) => Err(SubmoduleError::Io {
			path: PathBuf::from(name),
			source,
		}),
	}
}

fn sync_directory(directory: &Dir, display: &Path) -> Result<(), SubmoduleError> {
	let mut options = OpenOptions::new();
	#[cfg(not(windows))]
	options.read(true);
	#[cfg(windows)]
	{
		use cap_std::fs::OpenOptionsExt as _;
		use windows_sys::Win32::Storage::FileSystem::{
			FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
		};
		options
			.read(true)
			.write(true)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
	}
	directory
		.open_with(".", &options)
		.and_then(|file| file.sync_all())
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})
}

fn sync_destination_then_source(
	mut destination: impl FnMut() -> Result<(), SubmoduleError>,
	mut source: impl FnMut() -> Result<(), SubmoduleError>,
) -> Result<(), SubmoduleError> {
	// If an interruption lands between barriers, retaining both names is recoverable; durably
	// removing the source before its destination exists is not.
	destination()?;
	source()
}

fn path_string(path: &Path) -> Result<String, SubmoduleError> {
	path
		.to_str()
		.map(str::to_owned)
		.ok_or_else(|| SubmoduleError::UnsafePath(path.display().to_string()))
}

fn os_string(value: &OsStr) -> Result<String, SubmoduleError> {
	value
		.to_str()
		.map(str::to_owned)
		.ok_or_else(|| SubmoduleError::UnsafePath(value.to_string_lossy().into_owned()))
}

fn private_component(value: &str, purposes: &[&str]) -> bool {
	let mut components = Path::new(value).components();
	let Some(Component::Normal(component)) = components.next() else {
		return false;
	};
	components.next().is_none()
		&& purposes.iter().any(|purpose| {
			component
				.to_string_lossy()
				.starts_with(&format!(".gitana-submodule-deinit-{purpose}."))
		})
}

fn private_config_component(value: &str) -> bool {
	let mut components = Path::new(value).components();
	let Some(Component::Normal(component)) = components.next() else {
		return false;
	};
	components.next().is_none()
		&& component
			.to_string_lossy()
			.starts_with(".gitana-config-prepared.")
}

#[cfg(all(test, unix))]
mod tests {
	use super::{
		DeinitIntent, DeinitPhase, INTENT_NAME, INTENT_PREVIOUS_NAME, INTENT_VERSION,
		RetirementLocation, SubmoduleContext, deinit_query, deinit_recovery_git_dirs,
		ensure_distinct_deinit_config_targets, ensure_named_intent_identity,
		promote_windows_intent_predecessor, read_named_intent_file, read_windows_intent_file,
		same_deinit_transaction, sync_destination_then_source,
	};
	use crate::{
		ConfigViews, DeinitConfigPublication, DeinitConfigTarget, DeinitConfigTransition,
		DeinitMountMarker, DeinitSelection, DurableIdentity, MarkerTargetResolver,
		SubmoduleDeclaration, SubmoduleError,
	};
	use cap_fs_ext::DirExt as _;
	use cap_std::{ambient_authority, fs::Dir};
	use gitana_config::GitConfig;
	use gitana_fs_native::{
		EntryIdentity, directory_identity, entry_identity, rename_noreplace_if_identity,
	};
	use gitana_object::HashKind;
	use gitana_repository_layout::RepositoryLayout;
	use std::ffi::OsStr;
	use std::path::Path;

	fn journal_intent(phase: DeinitPhase) -> DeinitIntent {
		DeinitIntent {
			version: INTENT_VERSION,
			phase,
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			recorded: "1".repeat(40),
			force: false,
			core_worktree: "../../../modules/one".to_owned(),
			module_transition: None,
			module_publication: None,
			super_transition: DeinitConfigTransition {
				before_fingerprint: "before".to_owned(),
				after_fingerprint: "after".to_owned(),
				target: DeinitConfigTarget::new((1, 2), Some((1, 3)), Vec::new()),
				worktree_attachment: None,
			},
			super_publication: None,
			parent: "modules".to_owned(),
			target: "one".to_owned(),
			prepared: None,
			displaced: None,
			mount_identity: None,
			mount_marker: None,
			prepared_identity: None,
			public_identity: gitana_fs_native::EntryIdentity::from_parts(1, 4).into(),
			module_identity: None,
			retired: None,
			rollback: None,
			retirement_location: None,
		}
	}

	#[test]
	fn deinit_selection_requires_explicit_all_or_nonempty_paths() {
		assert!(
			deinit_query(&DeinitSelection::All)
				.unwrap()
				.pathspecs
				.is_empty()
		);
		assert!(matches!(
			deinit_query(&DeinitSelection::Paths(Vec::new())),
			Err(SubmoduleError::EmptyDeinitSelection)
		));
		assert_eq!(
			deinit_query(&DeinitSelection::Paths(vec!["modules/one".to_owned()]))
				.unwrap()
				.pathspecs,
			["modules/one"]
		);
	}

	#[test]
	fn recovery_enumeration_rejects_a_symlinked_worktrees_container() {
		use std::os::unix::fs::symlink;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let relocated = temporary.path().join("relocated-worktrees");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::create_dir(&relocated).unwrap();
		symlink(&relocated, common_path.join("worktrees")).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let layout = RepositoryLayout {
			worktree_root: Some(temporary.path().join("work")),
			git_dir: common_path.clone(),
			common_dir: common_path,
		};

		let error = deinit_recovery_git_dirs(&common, &common, &layout).unwrap_err();
		assert!(matches!(error, SubmoduleError::RecoveryRequired(_)));
	}

	#[test]
	fn recovery_enumeration_rejects_a_symlinked_worktree_owner() {
		use std::os::unix::fs::symlink;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let worktrees = common_path.join("worktrees");
		let relocated = temporary.path().join("relocated-admin");
		std::fs::create_dir_all(&worktrees).unwrap();
		std::fs::create_dir(&relocated).unwrap();
		symlink(&relocated, worktrees.join("linked")).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let layout = RepositoryLayout {
			worktree_root: Some(temporary.path().join("work")),
			git_dir: common_path.clone(),
			common_dir: common_path,
		};

		let error = deinit_recovery_git_dirs(&common, &common, &layout).unwrap_err();
		assert!(matches!(error, SubmoduleError::RecoveryRequired(_)));
	}

	#[test]
	fn deinit_config_targets_must_not_alias() {
		let first = DeinitConfigTarget::new((1, 2), Some((1, 3)), vec![(1, 4)]);
		let alias = DeinitConfigTarget::new((5, 6), Some((1, 3)), vec![(5, 7)]);
		let distinct = DeinitConfigTarget::new((1, 2), Some((1, 8)), vec![(1, 4)]);

		assert!(matches!(
			ensure_distinct_deinit_config_targets(&[
				("module 'one'".to_owned(), first.clone()),
				("module 'two'".to_owned(), alias),
			]),
			Err(SubmoduleError::Configuration(message))
				if message.contains("module 'one'") && message.contains("module 'two'")
		));
		ensure_distinct_deinit_config_targets(&[
			("module 'one'".to_owned(), first),
			("superproject".to_owned(), distinct),
		])
		.unwrap();
	}

	#[test]
	fn checkout_retirement_flushes_destination_before_source() {
		let synced = std::cell::RefCell::new(Vec::new());
		sync_destination_then_source(
			|| {
				synced.borrow_mut().push("destination");
				Ok(())
			},
			|| {
				synced.borrow_mut().push("source");
				Ok(())
			},
		)
		.unwrap();
		assert_eq!(*synced.borrow(), ["destination", "source"]);
	}

	#[test]
	fn checkout_retirement_does_not_flush_source_after_destination_failure() {
		let synced = std::cell::RefCell::new(Vec::new());
		let result = sync_destination_then_source(
			|| {
				synced.borrow_mut().push("destination");
				Err(SubmoduleError::Configuration(
					"destination sync failed".to_owned(),
				))
			},
			|| {
				synced.borrow_mut().push("source");
				Ok(())
			},
		);
		assert!(result.is_err());
		assert_eq!(*synced.borrow(), ["destination"]);
	}

	#[test]
	fn windows_journal_recovery_restores_a_lone_predecessor() {
		let temporary = tempfile::tempdir().unwrap();
		let previous = journal_intent(DeinitPhase::Detaching);
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);

		let (recovered, _) = read_windows_intent_file(&control, &display)
			.unwrap()
			.expect("the predecessor must be recovered");
		assert_eq!(recovered.phase, DeinitPhase::Detaching);
		assert!(temporary.path().join(INTENT_NAME).is_file());
		assert!(!temporary.path().join(INTENT_PREVIOUS_NAME).exists());
	}

	#[test]
	fn windows_journal_recovery_accepts_a_concurrent_predecessor_promotion() {
		let temporary = tempfile::tempdir().unwrap();
		let previous = journal_intent(DeinitPhase::Detaching);
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);
		let (observed, identity) = read_named_intent_file(
			&control,
			OsStr::new(INTENT_PREVIOUS_NAME),
			&display.with_file_name(INTENT_PREVIOUS_NAME),
		)
		.unwrap()
		.unwrap();
		rename_noreplace_if_identity(
			&control,
			OsStr::new(INTENT_PREVIOUS_NAME),
			identity,
			&control,
			OsStr::new(INTENT_NAME),
		)
		.unwrap();

		let (recovered, recovered_identity) =
			promote_windows_intent_predecessor(&control, &display, observed, identity).unwrap();
		assert_eq!(recovered, previous);
		assert_eq!(recovered_identity, identity);
		assert!(temporary.path().join(INTENT_NAME).is_file());
		assert!(!temporary.path().join(INTENT_PREVIOUS_NAME).exists());
	}

	#[test]
	fn windows_journal_recovery_rejects_changed_content_after_a_promotion_race() {
		let temporary = tempfile::tempdir().unwrap();
		let previous = journal_intent(DeinitPhase::Detaching);
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);
		let (observed, identity) = read_named_intent_file(
			&control,
			OsStr::new(INTENT_PREVIOUS_NAME),
			&display.with_file_name(INTENT_PREVIOUS_NAME),
		)
		.unwrap()
		.unwrap();
		rename_noreplace_if_identity(
			&control,
			OsStr::new(INTENT_PREVIOUS_NAME),
			identity,
			&control,
			OsStr::new(INTENT_NAME),
		)
		.unwrap();
		let mut changed = previous;
		changed.name = "foreign".to_owned();
		control
			.write(INTENT_NAME, serde_json::to_vec(&changed).unwrap())
			.unwrap();
		assert_eq!(
			entry_identity(&control, OsStr::new(INTENT_NAME)).unwrap(),
			identity
		);

		assert!(matches!(
			promote_windows_intent_predecessor(&control, &display, observed, identity),
			Err(SubmoduleError::Io { .. })
		));
	}

	#[test]
	fn journal_predecessors_must_belong_to_the_same_transaction() {
		let previous = journal_intent(DeinitPhase::Prepared);
		let mut current = journal_intent(DeinitPhase::Detaching);
		assert!(same_deinit_transaction(&previous, &current));
		current.name = "foreign".to_owned();
		assert!(!same_deinit_transaction(&previous, &current));
	}

	#[test]
	fn journal_predecessor_proofs_must_be_preserved() {
		let previous = journal_intent(DeinitPhase::Prepared);
		let mut current = journal_intent(DeinitPhase::SuperConfigReserved);
		current.mount_marker = Some(DeinitMountMarker {
			identity: DurableIdentity::from(EntryIdentity::from_parts(1, 5)),
			bytes: b"gitdir: module\n".to_vec(),
		});
		current.module_publication = Some(DeinitConfigPublication::new(
			"module-prepared".to_owned(),
			1,
			6,
		));
		current.super_publication = Some(DeinitConfigPublication::new(
			"super-prepared".to_owned(),
			1,
			7,
		));
		current.retirement_location = Some(RetirementLocation::ModuleRepository);
		assert!(
			same_deinit_transaction(&previous, &current),
			"a successor may introduce a proof at its journaled phase"
		);

		let previous = current;
		let mut changed = previous.clone();
		changed.mount_marker.as_mut().unwrap().bytes.push(b'!');
		assert!(!same_deinit_transaction(&previous, &changed));

		let mut changed = previous.clone();
		changed.module_publication = Some(DeinitConfigPublication::new(
			"module-prepared".to_owned(),
			1,
			8,
		));
		assert!(!same_deinit_transaction(&previous, &changed));

		let mut changed = previous.clone();
		changed.super_publication = None;
		assert!(!same_deinit_transaction(&previous, &changed));

		let mut changed = previous.clone();
		changed.retirement_location = Some(RetirementLocation::WorktreeSibling);
		assert!(!same_deinit_transaction(&previous, &changed));
	}

	#[test]
	fn windows_journal_recovery_rejects_a_foreign_predecessor_immediately() {
		let temporary = tempfile::tempdir().unwrap();
		let mut previous = journal_intent(DeinitPhase::Prepared);
		previous.name = "foreign".to_owned();
		let current = journal_intent(DeinitPhase::Detaching);
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		std::fs::write(
			temporary.path().join(INTENT_NAME),
			serde_json::to_vec(&current).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);

		let error = read_windows_intent_file(&control, &display).unwrap_err();
		assert!(matches!(error, SubmoduleError::RecoveryRequired(_)));
		assert!(temporary.path().join(INTENT_NAME).is_file());
		assert!(temporary.path().join(INTENT_PREVIOUS_NAME).is_file());
	}

	#[test]
	fn windows_journal_recovery_rejects_a_phase_regression_immediately() {
		let temporary = tempfile::tempdir().unwrap();
		let previous = journal_intent(DeinitPhase::Detaching);
		let current = journal_intent(DeinitPhase::Prepared);
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		std::fs::write(
			temporary.path().join(INTENT_NAME),
			serde_json::to_vec(&current).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);

		let error = read_windows_intent_file(&control, &display).unwrap_err();
		assert!(matches!(error, SubmoduleError::RecoveryRequired(_)));
		assert!(temporary.path().join(INTENT_NAME).is_file());
		assert!(temporary.path().join(INTENT_PREVIOUS_NAME).is_file());
	}

	#[test]
	fn windows_journal_recovery_rejects_a_changed_predecessor_proof_immediately() {
		let temporary = tempfile::tempdir().unwrap();
		let mut previous = journal_intent(DeinitPhase::ModuleConfigReserved);
		previous.module_publication = Some(DeinitConfigPublication::new(
			"module-prepared".to_owned(),
			1,
			6,
		));
		let mut current = previous.clone();
		current.phase = DeinitPhase::ModuleConfigPrepared;
		current.module_publication = Some(DeinitConfigPublication::new(
			"module-prepared".to_owned(),
			1,
			7,
		));
		std::fs::write(
			temporary.path().join(INTENT_PREVIOUS_NAME),
			serde_json::to_vec(&previous).unwrap(),
		)
		.unwrap();
		std::fs::write(
			temporary.path().join(INTENT_NAME),
			serde_json::to_vec(&current).unwrap(),
		)
		.unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let display = temporary.path().join(INTENT_NAME);

		let error = read_windows_intent_file(&control, &display).unwrap_err();
		assert!(matches!(error, SubmoduleError::RecoveryRequired(_)));
		assert!(temporary.path().join(INTENT_NAME).is_file());
		assert!(temporary.path().join(INTENT_PREVIOUS_NAME).is_file());
	}

	#[test]
	fn stale_named_intent_identity_is_rejected_before_recovery() {
		let temporary = tempfile::tempdir().unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		control.write(INTENT_NAME, b"old").unwrap();
		let expected = entry_identity(&control, OsStr::new(INTENT_NAME)).unwrap();
		control.rename(INTENT_NAME, &control, "old-intent").unwrap();
		control.write(INTENT_NAME, b"new").unwrap();

		assert!(matches!(
			ensure_named_intent_identity(
				&control,
				OsStr::new(INTENT_NAME),
				expected,
				"changed"
			),
			Err(SubmoduleError::RecoveryRequired(message)) if message == "changed"
		));
	}

	fn mount_fixture(name: &str) -> (tempfile::TempDir, SubmoduleContext, SubmoduleDeclaration) {
		let temporary = tempfile::Builder::new().prefix(name).tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		let mount = worktree.join("modules/one");
		std::fs::create_dir_all(&mount).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("modules/one")).unwrap();
		std::fs::write(mount.join(".git"), "gitdir: ../../.git/modules/one\n").unwrap();
		let worktree = std::fs::canonicalize(worktree).unwrap();
		let git_dir = std::fs::canonicalize(git_dir).unwrap();
		let context = SubmoduleContext::new(
			RepositoryLayout {
				worktree_root: Some(worktree.clone()),
				git_dir: git_dir.clone(),
				common_dir: git_dir.clone(),
			},
			Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap(),
			Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap(),
			Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
			ConfigViews::new(GitConfig::parse("").unwrap()),
			String::new(),
			HashKind::Sha256,
		)
		.unwrap();
		let declaration = SubmoduleDeclaration {
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			url: None,
			branch: None,
			update: None,
			shallow: None,
		};
		(temporary, context, declaration)
	}

	struct TestMarkerResolver;

	impl MarkerTargetResolver for TestMarkerResolver {
		async fn marker_target_matches(
			&self,
			mount_path: &Path,
			target: &str,
			expected_git_dir: Dir,
		) -> Result<bool, SubmoduleError> {
			let target = Path::new(target);
			let resolved = if target.is_absolute() {
				target.to_owned()
			} else {
				mount_path.join(target)
			};
			let Ok(actual) = Dir::open_ambient_dir(resolved, ambient_authority()) else {
				return Ok(false);
			};
			Ok(matches!(
				(directory_identity(&actual), directory_identity(&expected_git_dir)),
				(Ok(actual), Ok(expected)) if actual == expected
			))
		}
	}

	#[derive(Clone, Copy)]
	enum MarkerMutation {
		Replace,
		Rewrite,
	}

	struct MutatingMarkerResolver {
		marker: std::path::PathBuf,
		mutation: MarkerMutation,
	}

	impl MarkerTargetResolver for MutatingMarkerResolver {
		async fn marker_target_matches(
			&self,
			mount_path: &Path,
			target: &str,
			expected_git_dir: Dir,
		) -> Result<bool, SubmoduleError> {
			match self.mutation {
				MarkerMutation::Replace => {
					let bytes = std::fs::read(&self.marker).unwrap();
					let replacement = self.marker.with_extension("gitana-replacement");
					std::fs::write(&replacement, bytes).unwrap();
					std::fs::rename(replacement, &self.marker).unwrap();
				}
				MarkerMutation::Rewrite => {
					std::fs::write(&self.marker, "gitdir: ../../.git/modules/./one\n").unwrap();
				}
			}
			TestMarkerResolver
				.marker_target_matches(mount_path, target, expected_git_dir)
				.await
		}
	}

	#[test]
	fn legacy_unix_preexchange_intent_is_normalized_before_mutation() {
		let (_temporary, context, declaration) = mount_fixture("deinit-legacy-normalize");
		let (parent, target, parent_display) = context.mount_parent(&declaration.path).unwrap();
		let mount = entry_identity(&parent, &target).unwrap();
		let prepared = context
			.create_private_empty_directory(&parent, "prepared", &parent_display)
			.unwrap();
		let empty = entry_identity(&parent, &prepared).unwrap();
		let module = context
			.open_git_subdir_nofollow(Path::new("modules/one"))
			.unwrap();
		let retired = context
			.private_retirement_name(
				&parent,
				&module,
				&parent_display,
				&context.layout.git_dir.join("modules/one"),
			)
			.unwrap();
		let prepared_name = prepared.to_string_lossy().into_owned();
		let mut intent = journal_intent(DeinitPhase::Prepared);
		intent.prepared = Some(prepared_name.clone());
		intent.displaced = Some(prepared_name);
		intent.mount_identity = Some(mount.into());
		intent.prepared_identity = Some(empty.into());
		intent.public_identity = mount.into();
		intent.module_identity = Some(directory_identity(&module).unwrap().into());
		intent.retired = Some(retired.to_string_lossy().into_owned());
		context.prepare_empty_control_dir().unwrap();
		let mut intent_identity = context.publish_deinit_intent(&intent).unwrap();

		context
			.normalize_legacy_unix_mount_intent(
				&mut intent,
				&mut intent_identity,
				&parent,
				&target,
				&parent_display,
			)
			.unwrap();

		assert_ne!(intent.prepared, intent.displaced);
		assert!(intent.rollback.is_some());
		assert_eq!(intent.phase, DeinitPhase::Prepared);
		assert_eq!(
			entry_identity(&parent, OsStr::new(intent.prepared.as_ref().unwrap())).unwrap(),
			empty
		);
		assert!(entry_identity(&parent, OsStr::new(intent.displaced.as_ref().unwrap())).is_err());
		super::validate_deinit_intent_shape(&intent).unwrap();
	}

	#[test]
	fn detachment_recovery_restores_a_raced_mount_source() {
		let (_temporary, context, declaration) = mount_fixture("deinit-raced-mount-source");
		let (parent, target, parent_display) = context.mount_parent(&declaration.path).unwrap();
		let mount = entry_identity(&parent, &target).unwrap();
		parent.rename(&target, &parent, "owned-away").unwrap();
		parent.create_dir("prepared").unwrap();
		let empty = entry_identity(&parent, OsStr::new("prepared")).unwrap();
		parent.create_dir("displaced").unwrap();
		let raced = entry_identity(&parent, OsStr::new("displaced")).unwrap();
		let mut intent = journal_intent(DeinitPhase::MountDisplaced);
		let mut intent_identity = mount;

		let error = context
			.ensure_mount_detached(
				&mut intent,
				&mut intent_identity,
				&parent,
				&target,
				OsStr::new("prepared"),
				OsStr::new("displaced"),
				empty,
				mount,
				&parent_display,
			)
			.unwrap_err();

		assert!(matches!(error, SubmoduleError::ForeignMount(_)));
		assert_eq!(entry_identity(&parent, &target).unwrap(), raced);
		assert!(parent.symlink_metadata("displaced").is_err());
		assert_eq!(
			entry_identity(&parent, OsStr::new("prepared")).unwrap(),
			empty
		);
	}

	#[test]
	fn detachment_recovery_restores_both_sources_after_a_raced_empty_source() {
		let (_temporary, context, declaration) = mount_fixture("deinit-raced-empty-source");
		let (parent, target, parent_display) = context.mount_parent(&declaration.path).unwrap();
		let mount = entry_identity(&parent, &target).unwrap();
		parent.create_dir("prepared").unwrap();
		let empty = entry_identity(&parent, OsStr::new("prepared")).unwrap();
		parent.rename(&target, &parent, "displaced").unwrap();
		parent.remove_dir("prepared").unwrap();
		parent.create_dir(&target).unwrap();
		let raced = entry_identity(&parent, &target).unwrap();
		let mut intent = journal_intent(DeinitPhase::MountDisplaced);
		let mut intent_identity = mount;

		let error = context
			.ensure_mount_detached(
				&mut intent,
				&mut intent_identity,
				&parent,
				&target,
				OsStr::new("prepared"),
				OsStr::new("displaced"),
				empty,
				mount,
				&parent_display,
			)
			.unwrap_err();

		assert!(matches!(error, SubmoduleError::ForeignMount(_)));
		assert_eq!(entry_identity(&parent, &target).unwrap(), mount);
		assert_eq!(
			entry_identity(&parent, OsStr::new("prepared")).unwrap(),
			raced
		);
		assert!(parent.symlink_metadata("displaced").is_err());
	}

	#[tokio::test]
	async fn marker_ownership_resolves_symlinks_before_parent_components() {
		use std::os::unix::fs::symlink;

		let (temporary, context, declaration) = mount_fixture("deinit-marker-resolution");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		let foreign = temporary.path().join("foreign");
		std::fs::create_dir_all(foreign.join("child")).unwrap();
		std::fs::create_dir_all(foreign.join(".git/modules/one")).unwrap();
		symlink(foreign.join("child"), worktree.join("trap")).unwrap();

		assert!(
			!context
				.marker_targets_expected(
					&declaration,
					"../../trap/../.git/modules/one",
					&TestMarkerResolver,
				)
				.await
				.unwrap()
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn relocated_mount_uses_its_recorded_marker_snapshot() {
		use std::os::unix::fs::symlink;

		let (_temporary, context, declaration) = mount_fixture("deinit-relocated-marker-alias");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		std::fs::create_dir(worktree.join(".git/modules/one/child")).unwrap();
		let marker_path = worktree.join("modules/one/.git");
		std::fs::write(&marker_path, "gitdir: jump/..\n").unwrap();
		symlink(
			"../../.git/modules/one/child",
			worktree.join("modules/one/jump"),
		)
		.unwrap();
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let (_, mount, marker) = context
			.capture_owned_mount(&declaration, &parent, &target, &TestMarkerResolver)
			.await
			.unwrap();
		let mut intent = journal_intent(DeinitPhase::MountDisplaced);
		intent.mount_identity = Some(mount.into());
		intent.mount_marker = Some(marker);
		parent.rename(&target, &parent, "displaced").unwrap();
		parent.create_dir(&target).unwrap();

		context
			.ensure_displaced_owned(
				&intent,
				&parent,
				OsStr::new("displaced"),
				mount,
				&TestMarkerResolver,
			)
			.await
			.unwrap();
	}

	#[tokio::test]
	async fn marker_capture_rejects_a_same_byte_replacement_during_resolution() {
		let (_temporary, context, declaration) = mount_fixture("deinit-marker-replacement");
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let resolver = MutatingMarkerResolver {
			marker: context
				.layout
				.worktree_root
				.as_ref()
				.unwrap()
				.join("modules/one/.git"),
			mutation: MarkerMutation::Replace,
		};

		assert!(matches!(
			context
				.capture_owned_mount(&declaration, &parent, &target, &resolver)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}

	#[tokio::test]
	async fn marker_capture_rejects_changed_bytes_during_resolution() {
		let (_temporary, context, declaration) = mount_fixture("deinit-marker-rewrite");
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let resolver = MutatingMarkerResolver {
			marker: context
				.layout
				.worktree_root
				.as_ref()
				.unwrap()
				.join("modules/one/.git"),
			mutation: MarkerMutation::Rewrite,
		};

		assert!(matches!(
			context
				.capture_owned_mount(&declaration, &parent, &target, &resolver)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn markerless_version_four_intent_is_normalized_before_recovery_mutation() {
		let (_temporary, context, declaration) = mount_fixture("deinit-marker-normalize");
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let mount = entry_identity(&parent, &target).unwrap();
		let mut intent = journal_intent(DeinitPhase::Prepared);
		intent.mount_identity = Some(mount.into());
		intent.public_identity = mount.into();
		context.prepare_empty_control_dir().unwrap();
		let mut intent_identity = context.publish_deinit_intent(&intent).unwrap();

		context
			.normalize_legacy_mount_marker_intent(
				&mut intent,
				&mut intent_identity,
				&parent,
				&target,
				None,
				&TestMarkerResolver,
			)
			.await
			.unwrap();

		let mount_dir = parent.open_dir_nofollow(&target).unwrap();
		let marker = intent.mount_marker.as_ref().unwrap();
		assert_eq!(marker.bytes, b"gitdir: ../../.git/modules/one\n");
		assert_eq!(
			EntryIdentity::from(marker.identity),
			entry_identity(&mount_dir, OsStr::new(".git")).unwrap()
		);
		let (persisted, _) = context.read_deinit_intent().unwrap().unwrap();
		assert_eq!(persisted.mount_marker, intent.mount_marker);
		assert_eq!(persisted.phase, DeinitPhase::Prepared);
	}

	#[tokio::test]
	async fn planned_mount_identity_rejects_a_valid_looking_replacement() {
		let (_temporary, context, declaration) = mount_fixture("deinit-mount-replacement");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let (_, expected, marker) = context
			.capture_owned_mount(&declaration, &parent, &target, &TestMarkerResolver)
			.await
			.unwrap();

		std::fs::rename(
			worktree.join("modules/one"),
			worktree.join("modules/original-one"),
		)
		.unwrap();
		std::fs::create_dir(worktree.join("modules/one")).unwrap();
		std::fs::write(
			worktree.join("modules/one/.git"),
			"gitdir: ../../.git/modules/one\n",
		)
		.unwrap();

		assert!(matches!(
			context
				.revalidate_owned_mount(
					&declaration,
					&parent,
					&target,
					expected,
					&marker,
					&TestMarkerResolver,
				)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}

	#[test]
	fn terminal_validation_reopens_a_replaced_public_parent() {
		let (_temporary, context, declaration) = mount_fixture("deinit-parent-replacement");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		std::fs::remove_file(worktree.join("modules/one/.git")).unwrap();
		let (retained_parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let expected = entry_identity(&retained_parent, OsStr::new(&target)).unwrap();
		context
			.ensure_empty_identity(&retained_parent, &target, expected, &declaration.path)
			.unwrap();

		std::fs::rename(worktree.join("modules"), worktree.join("original-modules")).unwrap();
		std::fs::create_dir_all(worktree.join("modules/one")).unwrap();

		context
			.ensure_empty_identity(&retained_parent, &target, expected, &declaration.path)
			.unwrap();
		let (current_parent, current_target, _) = context.mount_parent(&declaration.path).unwrap();
		assert!(matches!(
			context.ensure_empty_identity(
				&current_parent,
				&current_target,
				expected,
				&declaration.path,
			),
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}

	#[test]
	fn empty_validation_rejects_a_replacement_opened_after_the_name_check() {
		let (_temporary, context, declaration) = mount_fixture("deinit-empty-open-race");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		std::fs::remove_file(worktree.join("modules/one/.git")).unwrap();
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let expected = entry_identity(&parent, &target).unwrap();
		parent.rename(&target, &parent, "original-one").unwrap();
		parent.create_dir(&target).unwrap();
		let replacement = parent.open_dir_nofollow(&target).unwrap();

		assert!(matches!(
			context.validate_opened_empty_identity(
				&parent,
				&target,
				&replacement,
				expected,
				&declaration.path,
			),
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}

	#[test]
	fn empty_validation_rechecks_the_public_name_after_inspection() {
		let (_temporary, context, declaration) = mount_fixture("deinit-empty-name-race");
		let worktree = context.layout.worktree_root.as_ref().unwrap();
		std::fs::remove_file(worktree.join("modules/one/.git")).unwrap();
		let (parent, target, _) = context.mount_parent(&declaration.path).unwrap();
		let expected = entry_identity(&parent, &target).unwrap();
		let opened = parent.open_dir_nofollow(&target).unwrap();
		parent.rename(&target, &parent, "original-one").unwrap();
		parent.create_dir(&target).unwrap();

		assert!(matches!(
			context.validate_opened_empty_identity(
				&parent,
				&target,
				&opened,
				expected,
				&declaration.path,
			),
			Err(SubmoduleError::ForeignMount(path)) if path == declaration.path
		));
	}
}
