use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, OpenOptions};
use gitana_file_store_local::LocalFileStore;
#[cfg(windows)]
use gitana_fs_native::remove_file_if_identity;
#[cfg(not(windows))]
use gitana_fs_native::replace_if_identities;
use gitana_fs_native::{
	EntryIdentity, directory_identity, entry_identity, file_identity, rename_noreplace_if_identity,
};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_repository_layout::RepositoryLayout;
use serde::{Deserialize, Serialize};

use crate::{
	DeinitConfigPublication, DeinitConfigTransition, DeinitMountMarker, DurableIdentity, InitNotice,
	SetUrlConfigPath, SetUrlConfigurationProvider, SetUrlReport, SetUrlRequest, SetUrlValue,
	SubmoduleContext, SubmoduleDeclaration, SubmoduleError, SubmoduleMutationLease, UpdateLockGuard,
	acquire_update_lock_with_common, module_update_remote,
	try_acquire_submodule_config_mutation_lease, validate_name, validate_path,
};

const CONTROL_DIR: &str = "gitana-submodule-set-url";
const INTENT_NAME: &str = "intent.json";
const INTENT_LOCK_NAME: &str = "intent.lock";
#[cfg(windows)]
const INTENT_PREVIOUS_NAME: &str = "intent.previous";
const MODULE_CLAIM_NAME: &str = "gitana-submodule-set-url.claim";
const MODULE_CLAIM_LOCK_NAME: &str = "gitana-submodule-set-url.claim.lock";
const INTENT_VERSION: u32 = 1;
const PRIVATE_ATTEMPTS: u64 = 100;
static PRIVATE_COUNTER: AtomicU64 = AtomicU64::new(0);
static RETIRE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SetUrlConfig {
	transition: DeinitConfigTransition,
	publication: Option<DeinitConfigPublication>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SetUrlIntent {
	version: u32,
	transaction: String,
	phase: SetUrlPhase,
	name: String,
	path: String,
	declaration: SetUrlConfig,
	superproject: Option<SetUrlConfig>,
	module: Option<SetUrlConfig>,
	module_remote: Option<String>,
	module_identity: Option<DurableIdentity>,
	mount_identity: Option<DurableIdentity>,
	mount_marker: Option<DeinitMountMarker>,
	module_claim: Option<SetUrlModuleClaim>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SetUrlModuleClaim {
	version: u32,
	transaction: String,
	owner_identity: DurableIdentity,
	module_identity: DurableIdentity,
	name: String,
	path: String,
	identity: DurableIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SetUrlModuleClaimBody {
	version: u32,
	transaction: String,
	owner_identity: DurableIdentity,
	module_identity: DurableIdentity,
	name: String,
	path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SetUrlPhase {
	Planned,
	Ready,
	DeclarationApplied,
	SuperprojectApplied,
	ModuleApplied,
}

struct PlannedModule {
	directory: Dir,
	display_path: PathBuf,
	lease: SubmoduleMutationLease,
	remote: String,
	identity: EntryIdentity,
	mount_identity: EntryIdentity,
	mount_marker: DeinitMountMarker,
	transition: DeinitConfigTransition,
}

struct SetUrlPreparation<'a, C> {
	declared_url: &'a str,
	resolved_url: Option<&'a str>,
	module: Option<&'a PlannedModule>,
	configuration: &'a C,
	lock: &'a UpdateLockGuard,
}

impl SubmoduleContext {
	/// Set one declaration URL and durably synchronize registered configuration.
	pub async fn set_url<C: SetUrlConfigurationProvider>(
		&self,
		request: &SetUrlRequest,
		configuration: &C,
	) -> Result<SetUrlReport, SubmoduleError> {
		if gitana_remote::redact_password(&request.url) != request.url {
			return Err(SubmoduleError::Configuration(
				"submodule URLs containing a password are not persisted".to_owned(),
			));
		}
		match self.hash_kind {
			HashKind::Sha1 => self.set_url_typed::<Sha1, C>(request, configuration).await,
			HashKind::Sha256 => {
				self
					.set_url_typed::<Sha256, C>(request, configuration)
					.await
			}
		}
	}

	async fn set_url_typed<H: HashAlgorithm, C: SetUrlConfigurationProvider>(
		&self,
		request: &SetUrlRequest,
		configuration: &C,
	) -> Result<SetUrlReport, SubmoduleError> {
		let lock = self.acquire_config_update_lock()?;
		lock.validate()?;
		self.revalidate_set_url_superproject(configuration).await?;
		if repository_has_set_url_participant_claim(&self.git, &self.layout.git_dir)? {
			return Err(SubmoduleError::RecoveryRequired(
				"a parent submodule set-url must be retried from its owning superproject".to_owned(),
			));
		}
		if let Some((intent, identity)) = self.read_set_url_intent()? {
			self
				.complete_or_abandon_set_url(intent, identity, configuration, &lock)
				.await?;
		} else {
			self.retire_unpublished_set_url_control()?;
		}
		self.ensure_no_repository_deinit_recovery()?;
		self.ensure_no_update_recovery()?;

		let declaration_path = self.worktree_root().join(".gitmodules");
		let (name, declaration_transition) = configuration
			.plan_set_url_declaration(
				self.clone_dir(&self.work, self.worktree_root())?,
				&declaration_path,
				&request.path,
				&request.url,
			)
			.await?;
		validate_name(&name)?;
		validate_path(&request.path)?;
		let declaration = SubmoduleDeclaration {
			name: name.clone(),
			path: request.path.clone(),
			url: Some(request.url.clone()),
			branch: None,
			update: None,
			shallow: None,
		};
		self.clear_owned_orphaned_set_url_module_claim(&declaration)?;

		let effective = configuration.reload().await?;
		let registered = match effective.get_raw("submodule", Some(&name), "url") {
			Some(Some(_)) => true,
			Some(None) => {
				return Err(SubmoduleError::MissingValue(format!(
					"submodule.{name}.url"
				)));
			}
			None => false,
		};
		let mut notices = Vec::new();
		let resolved_url = if registered {
			let resolved = if request.url.starts_with("./") || request.url.starts_with("../") {
				let worktree = self.worktree::<H>()?;
				let base = self.branch_remote_base(&worktree, &effective).await?;
				if let Some(missing_key) = base.missing_remote_key {
					notices.push(InitNotice::AuthoritativeSuperproject { missing_key });
				}
				crate::resolve_relative_url(&base.url, &request.url).map_err(|_| {
					SubmoduleError::InvalidRelativeUrl {
						path: request.path.clone(),
						url: request.url.clone(),
					}
				})?
			} else {
				request.url.clone()
			};
			Some(gitana_remote::redact_password(&resolved))
		} else {
			None
		};

		let superproject_transition = if let Some(url) = resolved_url.as_deref() {
			Some(
				configuration
					.plan_set_url_value(
						self.clone_dir(&self.common, &self.layout.common_dir)?,
						&self.layout.common_dir.join("config"),
						&effective,
						"submodule",
						&name,
						url,
					)
					.await?,
			)
		} else {
			None
		};

		let module = if registered && self.mount_is_attached(&declaration)? {
			Some(
				self
					.plan_set_url_module::<H, C>(
						&declaration,
						resolved_url
							.as_deref()
							.expect("registered URL was resolved"),
						configuration,
					)
					.await?,
			)
		} else {
			None
		};

		let module_remote = module.as_ref().map(|module| module.remote.clone());
		let transaction = set_url_transaction_id(&self.git, &self.layout.git_dir)?;
		let mut intent = SetUrlIntent {
			version: INTENT_VERSION,
			transaction,
			phase: SetUrlPhase::Planned,
			name: name.clone(),
			path: request.path.clone(),
			declaration: SetUrlConfig {
				transition: declaration_transition,
				publication: None,
			},
			superproject: superproject_transition.map(|transition| SetUrlConfig {
				transition,
				publication: None,
			}),
			module: module.as_ref().map(|module| SetUrlConfig {
				transition: module.transition.clone(),
				publication: None,
			}),
			module_remote: module_remote.clone(),
			module_identity: module.as_ref().map(|module| module.identity.into()),
			mount_identity: module.as_ref().map(|module| module.mount_identity.into()),
			mount_marker: module.as_ref().map(|module| module.mount_marker.clone()),
			module_claim: None,
		};
		ensure_distinct_set_url_targets(&intent)?;
		let changed = intent.declaration.transition.changes()
			|| intent
				.superproject
				.as_ref()
				.is_some_and(|config| config.transition.changes())
			|| intent
				.module
				.as_ref()
				.is_some_and(|config| config.transition.changes());
		if changed {
			if let Some(module) = module.as_ref() {
				self.revalidate_set_url_module_namespace(&intent, module)?;
				self.revalidate_set_url_superproject(configuration).await?;
				intent.module_claim = Some(self.publish_set_url_module_claim(&intent, module)?);
			}
			self.revalidate_set_url_superproject(configuration).await?;
			let mut intent_identity = self.publish_set_url_intent(&intent)?;
			self
				.reserve_set_url_participants(
					&mut intent,
					&mut intent_identity,
					module.as_ref(),
					configuration,
					&lock,
				)
				.await?;
			self
				.prepare_set_url_participants(
					&intent,
					intent_identity,
					SetUrlPreparation {
						declared_url: &request.url,
						resolved_url: resolved_url.as_deref(),
						module: module.as_ref(),
						configuration,
						lock: &lock,
					},
				)
				.await?;
			if let Some(module) = module.as_ref() {
				self
					.revalidate_set_url_module(&intent, module, configuration)
					.await?;
			}
			intent_identity =
				self.advance_set_url_intent(&mut intent, intent_identity, SetUrlPhase::Ready)?;
			self
				.apply_set_url_participants(
					&mut intent,
					&mut intent_identity,
					module.as_ref(),
					configuration,
					&lock,
				)
				.await?;
			self
				.apply_set_url_participants(
					&mut intent,
					&mut intent_identity,
					module.as_ref(),
					configuration,
					&lock,
				)
				.await?;
			self
				.validate_set_url_effective_participants(&intent, module.as_ref(), configuration)
				.await?;
			self
				.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
				.await?;
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
			self.clear_set_url_intent(intent_identity)?;
			self.clear_set_url_module_claim(&intent, module.as_ref())?;
			self
				.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
				.await?;
		}
		self.revalidate_set_url_superproject(configuration).await?;
		lock.validate()?;
		Ok(SetUrlReport {
			name,
			path: request.path.clone(),
			declaration_changed: intent.declaration.transition.changes(),
			registration_synced: registered,
			module_remote,
			notices,
		})
	}

	async fn plan_set_url_module<H: HashAlgorithm, C: SetUrlConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		url: &str,
		configuration: &C,
	) -> Result<PlannedModule, SubmoduleError> {
		let (mount_parent, mount_target, _) = self.mount_parent(&declaration.path)?;
		let (mount, mount_identity, mount_marker) = self
			.capture_owned_mount(declaration, &mount_parent, &mount_target, configuration)
			.await?;
		let relative = Path::new("modules").join(&declaration.name);
		let display_path = self.layout.git_dir.join(&relative);
		let directory = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(declaration.name.clone()))?;
		let identity = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display_path.clone(),
			source,
		})?;
		let lease = try_acquire_submodule_config_mutation_lease(&directory, &display_path)?;
		if crate::repository_has_pending_update(&directory, &display_path)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule update recovery in module '{}' must be completed before changing its URL",
				declaration.name
			)));
		}
		let module_layout = RepositoryLayout {
			worktree_root: Some(self.worktree_root().join(&declaration.path)),
			git_dir: display_path.clone(),
			common_dir: display_path.clone(),
		};
		if crate::repository_has_pending_deinit(&directory, &directory, &module_layout)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule deinit recovery in module '{}' must be completed before changing its URL",
				declaration.name
			)));
		}
		if repository_has_pending_set_url_recovery(&directory, &directory, &module_layout)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending submodule set-url recovery in module '{}' must be completed before changing its URL",
				declaration.name
			)));
		}
		self
			.ensure_module_repository_valid::<H, C>(declaration, &directory, configuration)
			.await?;
		let config = configuration
			.load_module_config(
				directory.try_clone().map_err(|source| SubmoduleError::Io {
					path: display_path.clone(),
					source,
				})?,
				&display_path,
			)
			.await?;
		let files =
			LocalFileStore::from_dir(directory.try_clone().map_err(|source| SubmoduleError::Io {
				path: display_path.clone(),
				source,
			})?);
		let mut repository = Repository::<_, H>::new(ObjectStore::new(files));
		repository.set_effective_config(config.clone());
		let remote = module_update_remote(&repository, &config).await?;
		let transition = configuration
			.plan_set_url_value(
				directory.try_clone().map_err(|source| SubmoduleError::Io {
					path: display_path.clone(),
					source,
				})?,
				&display_path.join("config"),
				&config,
				"remote",
				&remote,
				url,
			)
			.await?;
		let (current_mount_parent, current_mount_target, _) = self.mount_parent(&declaration.path)?;
		self
			.revalidate_owned_mount(
				declaration,
				&current_mount_parent,
				&current_mount_target,
				mount_identity,
				&mount_marker,
				configuration,
			)
			.await?;
		if directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display_path.clone(),
			source,
		})? != identity
		{
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
		}
		drop(mount);
		Ok(PlannedModule {
			directory,
			display_path,
			lease,
			remote,
			identity,
			mount_identity,
			mount_marker,
			transition,
		})
	}

	fn clear_owned_orphaned_set_url_module_claim(
		&self,
		declaration: &SubmoduleDeclaration,
	) -> Result<(), SubmoduleError> {
		let relative = Path::new("modules").join(&declaration.name);
		let display_path = self.layout.git_dir.join(&relative);
		let directory = match self.open_git_subdir_nofollow(&relative) {
			Ok(directory) => directory,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
			Err(_) => return Err(SubmoduleError::InvalidRepository(declaration.name.clone())),
		};
		let identity = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display_path.clone(),
			source,
		})?;
		if read_module_claim(&directory, &display_path)?.is_none() {
			return Ok(());
		}

		let lease = try_acquire_submodule_config_mutation_lease(&directory, &display_path)?;
		lease.validate()?;
		self.reopen_module_directory(declaration, &relative, &display_path, identity)?;
		let owner_identity = directory_identity(&self.git).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.clone(),
			source,
		})?;
		clear_orphaned_set_url_module_claim(
			&directory,
			&display_path,
			owner_identity,
			identity,
			declaration,
		)?;
		self.reopen_module_directory(declaration, &relative, &display_path, identity)?;
		lease.validate()
	}

	async fn reserve_set_url_participants<C: SetUrlConfigurationProvider>(
		&self,
		intent: &mut SetUrlIntent,
		intent_identity: &mut EntryIdentity,
		module: Option<&PlannedModule>,
		configuration: &C,
		lock: &UpdateLockGuard,
	) -> Result<(), SubmoduleError> {
		ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
		self.revalidate_set_url_superproject(configuration).await?;
		if let Some(module) = module {
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
		}
		let declaration = configuration
			.reserve_set_url_config(
				self.clone_dir(&self.work, self.worktree_root())?,
				Path::new(".gitmodules"),
				&self.worktree_root().join(".gitmodules"),
				&intent.declaration.transition,
				lock.lease(),
			)
			.await?;
		intent.declaration.publication = declaration;
		*intent_identity =
			self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::Planned)?;

		if let Some(config) = intent.superproject.as_mut() {
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
			config.publication = configuration
				.reserve_set_url_config(
					self.clone_dir(&self.common, &self.layout.common_dir)?,
					Path::new("config"),
					&self.layout.common_dir.join("config"),
					&config.transition,
					lock.lease(),
				)
				.await?;
			*intent_identity =
				self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::Planned)?;
		}
		if let (Some(config), Some(module)) = (intent.module.as_mut(), module) {
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
			config.publication = configuration
				.reserve_set_url_config(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					Path::new("config"),
					&module.display_path.join("config"),
					&config.transition,
					lock.lease().combine(module.lease.clone()),
				)
				.await?;
			*intent_identity =
				self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::Planned)?;
		}
		Ok(())
	}

	async fn prepare_set_url_participants<C: SetUrlConfigurationProvider>(
		&self,
		intent: &SetUrlIntent,
		intent_identity: EntryIdentity,
		preparation: SetUrlPreparation<'_, C>,
	) -> Result<(), SubmoduleError> {
		ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
		if let Some(module) = preparation.module {
			self.validate_set_url_module_claim(intent, &module.directory, &module.display_path)?;
		}
		if let Some(publication) = &intent.declaration.publication {
			preparation
				.configuration
				.prepare_set_url_value(
					self.clone_dir(&self.work, self.worktree_root())?,
					SetUrlConfigPath {
						relative: Path::new(".gitmodules"),
						display: &self.worktree_root().join(".gitmodules"),
					},
					SetUrlValue {
						section: "submodule",
						subsection: &intent.name,
						url: preparation.declared_url,
					},
					&intent.declaration.transition,
					publication,
					preparation.lock.lease(),
				)
				.await?;
		}
		if let Some(config) = &intent.superproject
			&& let Some(publication) = &config.publication
		{
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
			preparation
				.configuration
				.prepare_set_url_value(
					self.clone_dir(&self.common, &self.layout.common_dir)?,
					SetUrlConfigPath {
						relative: Path::new("config"),
						display: &self.layout.common_dir.join("config"),
					},
					SetUrlValue {
						section: "submodule",
						subsection: &intent.name,
						url: preparation
							.resolved_url
							.expect("registered transition has a resolved URL"),
					},
					&config.transition,
					publication,
					preparation.lock.lease(),
				)
				.await?;
		}
		if let (Some(config), Some(module)) = (&intent.module, preparation.module)
			&& let Some(publication) = &config.publication
		{
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
			preparation
				.configuration
				.prepare_set_url_value(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					SetUrlConfigPath {
						relative: Path::new("config"),
						display: &module.display_path.join("config"),
					},
					SetUrlValue {
						section: "remote",
						subsection: &module.remote,
						url: preparation
							.resolved_url
							.expect("module transition has a resolved URL"),
					},
					&config.transition,
					publication,
					preparation.lock.lease().combine(module.lease.clone()),
				)
				.await?;
		}
		Ok(())
	}

	async fn apply_set_url_participants<C: SetUrlConfigurationProvider>(
		&self,
		intent: &mut SetUrlIntent,
		intent_identity: &mut EntryIdentity,
		module: Option<&PlannedModule>,
		configuration: &C,
		lock: &UpdateLockGuard,
	) -> Result<(), SubmoduleError> {
		ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
		if let Some(module) = module {
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
		}
		configuration
			.apply_set_url_config(
				self.clone_dir(&self.work, self.worktree_root())?,
				Path::new(".gitmodules"),
				&self.worktree_root().join(".gitmodules"),
				&intent.declaration.transition,
				intent.declaration.publication.as_ref(),
				lock.lease(),
			)
			.await?;
		self.revalidate_set_url_superproject(configuration).await?;
		if intent.phase < SetUrlPhase::DeclarationApplied {
			*intent_identity =
				self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::DeclarationApplied)?;
		}
		if let Some(config) = &intent.superproject {
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
			self.revalidate_set_url_superproject(configuration).await?;
			configuration
				.apply_set_url_config(
					self.clone_dir(&self.common, &self.layout.common_dir)?,
					Path::new("config"),
					&self.layout.common_dir.join("config"),
					&config.transition,
					config.publication.as_ref(),
					lock.lease(),
				)
				.await?;
			self.revalidate_set_url_superproject(configuration).await?;
		}
		if intent.phase < SetUrlPhase::SuperprojectApplied {
			*intent_identity =
				self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::SuperprojectApplied)?;
		}
		if let (Some(config), Some(module)) = (&intent.module, module) {
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, *intent_identity)?;
			self.revalidate_set_url_superproject(configuration).await?;
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
			self.revalidate_set_url_superproject(configuration).await?;
			configuration
				.apply_set_url_config(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					Path::new("config"),
					&module.display_path.join("config"),
					&config.transition,
					config.publication.as_ref(),
					lock.lease().combine(module.lease.clone()),
				)
				.await?;
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
		}
		if intent.phase < SetUrlPhase::ModuleApplied {
			*intent_identity =
				self.advance_set_url_intent(intent, *intent_identity, SetUrlPhase::ModuleApplied)?;
		}
		Ok(())
	}

	async fn revalidate_set_url_module<C: SetUrlConfigurationProvider>(
		&self,
		intent: &SetUrlIntent,
		module: &PlannedModule,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		self.revalidate_set_url_module_namespace(intent, module)?;
		self.validate_set_url_module_claim(intent, &module.directory, &module.display_path)?;
		let declaration = intent_declaration(intent);
		let (mount_parent, mount_target, _) = self.mount_parent(&intent.path)?;
		self
			.revalidate_owned_mount(
				&declaration,
				&mount_parent,
				&mount_target,
				module.mount_identity,
				&module.mount_marker,
				configuration,
			)
			.await?;
		self.revalidate_set_url_module_namespace(intent, module)
	}

	fn revalidate_set_url_module_namespace(
		&self,
		intent: &SetUrlIntent,
		module: &PlannedModule,
	) -> Result<(), SubmoduleError> {
		if intent.module_identity != Some(module.identity.into())
			|| directory_identity(&module.directory).map_err(|source| SubmoduleError::Io {
				path: module.display_path.clone(),
				source,
			})? != module.identity
		{
			return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
		}
		let declaration = intent_declaration(intent);
		let relative = Path::new("modules").join(&intent.name);
		self.reopen_module_directory(
			&declaration,
			&relative,
			&module.display_path,
			module.identity,
		)?;
		Ok(())
	}

	async fn revalidate_set_url_superproject<C: SetUrlConfigurationProvider>(
		&self,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		configuration
			.revalidate_set_url_superproject(
				self.clone_dir(&self.work, self.worktree_root())?,
				self.worktree_root(),
			)
			.await
	}

	async fn revalidate_set_url_completion<C: SetUrlConfigurationProvider>(
		&self,
		intent: &SetUrlIntent,
		module: Option<&PlannedModule>,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		self.revalidate_set_url_superproject(configuration).await?;
		if let Some(module) = module {
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
		}
		Ok(())
	}

	async fn validate_set_url_effective_participants<C: SetUrlConfigurationProvider>(
		&self,
		intent: &SetUrlIntent,
		module: Option<&PlannedModule>,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		if let Some(config) = &intent.superproject {
			let effective = configuration.reload().await?;
			configuration
				.validate_set_url_effective_value(
					self.clone_dir(&self.common, &self.layout.common_dir)?,
					SetUrlConfigPath {
						relative: Path::new("config"),
						display: &self.layout.common_dir.join("config"),
					},
					&effective,
					("submodule", &intent.name),
					&config.transition,
					config.publication.as_ref(),
				)
				.await?;
		}
		if let (Some(config), Some(module)) = (&intent.module, module) {
			self
				.revalidate_set_url_module(intent, module, configuration)
				.await?;
			let effective = configuration
				.load_module_config(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					&module.display_path,
				)
				.await?;
			configuration
				.validate_set_url_effective_value(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					SetUrlConfigPath {
						relative: Path::new("config"),
						display: &module.display_path.join("config"),
					},
					&effective,
					("remote", &module.remote),
					&config.transition,
					config.publication.as_ref(),
				)
				.await?;
		}
		Ok(())
	}

	async fn complete_or_abandon_set_url<C: SetUrlConfigurationProvider>(
		&self,
		mut intent: SetUrlIntent,
		mut identity: EntryIdentity,
		configuration: &C,
		lock: &UpdateLockGuard,
	) -> Result<(), SubmoduleError> {
		validate_set_url_intent(&intent)?;
		self.revalidate_set_url_superproject(configuration).await?;
		let module = self.open_recorded_set_url_module(&intent)?;
		if intent.phase < SetUrlPhase::Ready {
			self
				.discard_set_url_participants(&intent, identity, module.as_ref(), configuration, lock)
				.await?;
			self
				.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
				.await?;
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, identity)?;
			self.clear_set_url_intent(identity)?;
			self.clear_set_url_module_claim(&intent, module.as_ref())?;
			self
				.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
				.await?;
			return Ok(());
		}
		self
			.apply_set_url_participants(
				&mut intent,
				&mut identity,
				module.as_ref(),
				configuration,
				lock,
			)
			.await?;
		self
			.apply_set_url_participants(
				&mut intent,
				&mut identity,
				module.as_ref(),
				configuration,
				lock,
			)
			.await?;
		self
			.validate_set_url_effective_participants(&intent, module.as_ref(), configuration)
			.await?;
		self
			.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
			.await?;
		ensure_active_set_url_intent(&self.git, &self.layout.git_dir, identity)?;
		self.clear_set_url_intent(identity)?;
		self.clear_set_url_module_claim(&intent, module.as_ref())?;
		self
			.revalidate_set_url_completion(&intent, module.as_ref(), configuration)
			.await
	}

	fn open_recorded_set_url_module(
		&self,
		intent: &SetUrlIntent,
	) -> Result<Option<PlannedModule>, SubmoduleError> {
		let Some(expected) = intent.module_identity else {
			return Ok(None);
		};
		let remote = intent.module_remote.clone().ok_or_else(|| {
			SubmoduleError::RecoveryRequired("set-url module has no recorded remote".to_owned())
		})?;
		let relative = Path::new("modules").join(&intent.name);
		let display_path = self.layout.git_dir.join(&relative);
		let directory = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(intent.name.clone()))?;
		let identity = directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display_path.clone(),
			source,
		})?;
		if identity != EntryIdentity::from(expected) {
			return Err(SubmoduleError::InvalidRepository(intent.name.clone()));
		}
		let lease = try_acquire_submodule_config_mutation_lease(&directory, &display_path)?;
		self.validate_set_url_module_claim(intent, &directory, &display_path)?;
		let mount_identity = EntryIdentity::from(intent.mount_identity.ok_or_else(|| {
			SubmoduleError::RecoveryRequired("set-url module has no mount identity".to_owned())
		})?);
		let mount_marker = intent.mount_marker.clone().ok_or_else(|| {
			SubmoduleError::RecoveryRequired("set-url module has no mount marker".to_owned())
		})?;
		Ok(Some(PlannedModule {
			directory,
			display_path,
			lease,
			remote,
			identity,
			mount_identity,
			mount_marker,
			transition: intent
				.module
				.as_ref()
				.expect("recorded module identity has a transition")
				.transition
				.clone(),
		}))
	}

	fn publish_set_url_module_claim(
		&self,
		intent: &SetUrlIntent,
		module: &PlannedModule,
	) -> Result<SetUrlModuleClaim, SubmoduleError> {
		let body = SetUrlModuleClaimBody {
			version: INTENT_VERSION,
			transaction: intent.transaction.clone(),
			owner_identity: directory_identity(&self.git)
				.map_err(|source| SubmoduleError::Io {
					path: self.layout.git_dir.clone(),
					source,
				})?
				.into(),
			module_identity: module.identity.into(),
			name: intent.name.clone(),
			path: intent.path.clone(),
		};
		let identity = write_new_module_claim(&module.directory, &module.display_path, &body)?;
		Ok(SetUrlModuleClaim {
			version: body.version,
			transaction: body.transaction,
			owner_identity: body.owner_identity,
			module_identity: body.module_identity,
			name: body.name,
			path: body.path,
			identity: identity.into(),
		})
	}

	fn validate_set_url_module_claim(
		&self,
		intent: &SetUrlIntent,
		module: &Dir,
		display: &Path,
	) -> Result<(), SubmoduleError> {
		validate_recorded_module_claim(intent, &self.git, &self.layout.git_dir, module, display)
	}

	fn clear_set_url_module_claim(
		&self,
		intent: &SetUrlIntent,
		module: Option<&PlannedModule>,
	) -> Result<(), SubmoduleError> {
		let (Some(claim), Some(module)) = (&intent.module_claim, module) else {
			return Ok(());
		};
		match read_module_claim(&module.directory, &module.display_path)? {
			Some((body, identity))
				if body == module_claim_body(claim) && identity == EntryIdentity::from(claim.identity) =>
			{
				gitana_fs_native::remove_file_if_identity(
					&module.directory,
					OsStr::new(MODULE_CLAIM_NAME),
					identity,
				)
				.map_err(|source| SubmoduleError::Io {
					path: module.display_path.join(MODULE_CLAIM_NAME),
					source,
				})?;
				sync_directory(&module.directory, &module.display_path)
			}
			None => sync_directory(&module.directory, &module.display_path),
			Some(_) => Err(SubmoduleError::RecoveryRequired(
				"set-url module claim changed before retirement".to_owned(),
			)),
		}
	}

	async fn discard_set_url_participants<C: SetUrlConfigurationProvider>(
		&self,
		intent: &SetUrlIntent,
		intent_identity: EntryIdentity,
		module: Option<&PlannedModule>,
		configuration: &C,
		lock: &UpdateLockGuard,
	) -> Result<(), SubmoduleError> {
		ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
		if let Some(module) = module {
			self.validate_set_url_module_claim(intent, &module.directory, &module.display_path)?;
		}
		if let Some(publication) = &intent.declaration.publication {
			configuration
				.discard_set_url_config(
					self.clone_dir(&self.work, self.worktree_root())?,
					Path::new(".gitmodules"),
					&self.worktree_root().join(".gitmodules"),
					&intent.declaration.transition,
					publication,
					lock.lease(),
				)
				.await?;
		}
		if let Some(config) = &intent.superproject
			&& let Some(publication) = &config.publication
		{
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
			configuration
				.discard_set_url_config(
					self.clone_dir(&self.common, &self.layout.common_dir)?,
					Path::new("config"),
					&self.layout.common_dir.join("config"),
					&config.transition,
					publication,
					lock.lease(),
				)
				.await?;
		}
		if let (Some(config), Some(module)) = (&intent.module, module)
			&& let Some(publication) = &config.publication
		{
			ensure_active_set_url_intent(&self.git, &self.layout.git_dir, intent_identity)?;
			configuration
				.discard_set_url_config(
					module
						.directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module.display_path.clone(),
							source,
						})?,
					Path::new("config"),
					&module.display_path.join("config"),
					&config.transition,
					publication,
					lock.lease().combine(module.lease.clone()),
				)
				.await?;
		}
		Ok(())
	}

	fn read_set_url_intent(&self) -> Result<Option<(SetUrlIntent, EntryIdentity)>, SubmoduleError> {
		read_set_url_intent_at(&self.git, &self.layout.git_dir)
	}

	fn publish_set_url_intent(&self, intent: &SetUrlIntent) -> Result<EntryIdentity, SubmoduleError> {
		self.retire_unpublished_set_url_control()?;
		self
			.git
			.create_dir(CONTROL_DIR)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		sync_directory(&self.git, &self.layout.git_dir)?;
		let control = open_subdir_nofollow(&self.git, Path::new(CONTROL_DIR)).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			}
		})?;
		write_new_intent(&control, &self.layout.git_dir.join(CONTROL_DIR), intent)
	}

	fn advance_set_url_intent(
		&self,
		intent: &mut SetUrlIntent,
		expected: EntryIdentity,
		phase: SetUrlPhase,
	) -> Result<EntryIdentity, SubmoduleError> {
		let control = open_subdir_nofollow(&self.git, Path::new(CONTROL_DIR)).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			}
		})?;
		intent.phase = phase;
		replace_intent(
			&control,
			&self.layout.git_dir.join(CONTROL_DIR),
			intent,
			expected,
		)
	}

	fn retire_unpublished_set_url_control(&self) -> Result<(), SubmoduleError> {
		match self.git.symlink_metadata(CONTROL_DIR) {
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
				if self.read_set_url_intent()?.is_some() {
					return Err(SubmoduleError::RecoveryRequired(
						"another submodule set-url intent is already pending".to_owned(),
					));
				}
				let control =
					open_subdir_nofollow(&self.git, Path::new(CONTROL_DIR)).map_err(|source| {
						SubmoduleError::Io {
							path: self.layout.git_dir.join(CONTROL_DIR),
							source,
						}
					})?;
				let identity = directory_identity(&control).map_err(|source| SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				})?;
				self.retire_set_url_control(identity)
			}
			Ok(_) => Err(SubmoduleError::RecoveryRequired(
				"submodule set-url control path is not a directory".to_owned(),
			)),
			Err(source) => Err(SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			}),
		}
	}

	fn clear_set_url_intent(&self, expected: EntryIdentity) -> Result<(), SubmoduleError> {
		let control = open_subdir_nofollow(&self.git, Path::new(CONTROL_DIR)).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			}
		})?;
		ensure_named_identity(&control, OsStr::new(INTENT_NAME), expected)?;
		let identity = directory_identity(&control).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR),
			source,
		})?;
		self.retire_set_url_control(identity)
	}

	fn retire_set_url_control(&self, expected: EntryIdentity) -> Result<(), SubmoduleError> {
		for _ in 0..PRIVATE_ATTEMPTS {
			let sequence = RETIRE_COUNTER.fetch_add(1, Ordering::Relaxed);
			let retired = format!(
				".gitana-submodule-set-url-retired.{}.{}",
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
				Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: self.layout.git_dir.join(CONTROL_DIR),
						source,
					});
				}
			}
		}
		Err(SubmoduleError::RecoveryRequired(
			"could not retire the completed set-url journal".to_owned(),
		))
	}
}

/// Whether this per-worktree Git directory owns active set-URL recovery state.
pub fn repository_has_pending_set_url(git: &Dir, git_dir: &Path) -> Result<bool, SubmoduleError> {
	let control_pending = match git.symlink_metadata(CONTROL_DIR) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
			let _ = read_set_url_intent_at(git, git_dir)?;
			true
		}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule set-url control path is not a directory".to_owned(),
			));
		}
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: git_dir.join(CONTROL_DIR),
				source,
			});
		}
	};
	let claim_pending = repository_has_set_url_participant_claim(git, git_dir)?;
	Ok(control_pending || claim_pending)
}

/// Whether any worktree in this repository owns active set-URL recovery state.
///
/// The module's common config lock must be held by callers that use this result to authorize a
/// config mutation. Linked-worktree enumeration fails closed when its namespace cannot be pinned.
pub(crate) fn repository_has_pending_set_url_recovery(
	common: &Dir,
	current: &Dir,
	layout: &RepositoryLayout,
) -> Result<bool, SubmoduleError> {
	for (git_dir, git) in crate::deinit_recovery_git_dirs(common, current, layout)? {
		if repository_has_pending_set_url(&git, &git_dir)? {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Whether this module repository is claimed by a parent set-URL transaction.
pub fn repository_has_set_url_participant_claim(
	git: &Dir,
	git_dir: &Path,
) -> Result<bool, SubmoduleError> {
	match git.symlink_metadata(MODULE_CLAIM_NAME) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
		Ok(_) => Err(SubmoduleError::RecoveryRequired(
			"submodule set-url participant claim is not a regular file".to_owned(),
		)),
		Err(source) => Err(SubmoduleError::Io {
			path: git_dir.join(MODULE_CLAIM_NAME),
			source,
		}),
	}
}

fn intent_declaration(intent: &SetUrlIntent) -> SubmoduleDeclaration {
	SubmoduleDeclaration {
		name: intent.name.clone(),
		path: intent.path.clone(),
		url: None,
		branch: None,
		update: None,
		shallow: None,
	}
}

fn set_url_transaction_id(git: &Dir, git_dir: &Path) -> Result<String, SubmoduleError> {
	let identity = directory_identity(git).map_err(|source| SubmoduleError::Io {
		path: git_dir.to_owned(),
		source,
	})?;
	let (device, inode) = identity.parts();
	let elapsed = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos();
	let sequence = PRIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
	Ok(format!(
		"{device:x}-{inode:x}-{:x}-{elapsed:x}-{sequence:x}",
		std::process::id()
	))
}

fn module_claim_body(claim: &SetUrlModuleClaim) -> SetUrlModuleClaimBody {
	SetUrlModuleClaimBody {
		version: claim.version,
		transaction: claim.transaction.clone(),
		owner_identity: claim.owner_identity,
		module_identity: claim.module_identity,
		name: claim.name.clone(),
		path: claim.path.clone(),
	}
}

fn validate_recorded_module_claim(
	intent: &SetUrlIntent,
	owner: &Dir,
	owner_display: &Path,
	module: &Dir,
	module_display: &Path,
) -> Result<(), SubmoduleError> {
	let claim = intent.module_claim.as_ref().ok_or_else(|| {
		SubmoduleError::RecoveryRequired("set-url module claim is missing".to_owned())
	})?;
	if EntryIdentity::from(claim.owner_identity)
		!= directory_identity(owner).map_err(|source| SubmoduleError::Io {
			path: owner_display.to_owned(),
			source,
		})? {
		return Err(SubmoduleError::RecoveryRequired(
			"set-url module claim belongs to another coordinator".to_owned(),
		));
	}
	match read_module_claim(module, module_display)? {
		Some((body, identity))
			if body == module_claim_body(claim) && identity == EntryIdentity::from(claim.identity) =>
		{
			Ok(())
		}
		None if intent.phase < SetUrlPhase::Ready || intent.phase == SetUrlPhase::ModuleApplied => {
			Ok(())
		}
		_ => Err(SubmoduleError::RecoveryRequired(
			"set-url module claim changed during recovery".to_owned(),
		)),
	}
}

fn write_new_module_claim(
	module: &Dir,
	display: &Path,
	claim: &SetUrlModuleClaimBody,
) -> Result<EntryIdentity, SubmoduleError> {
	clear_unpublished_module_claim(module, display)?;
	let bytes = serde_json::to_vec(claim).map_err(|error| {
		SubmoduleError::RecoveryRequired(format!("serializing set-url module claim: {error}"))
	})?;
	let mut options = OpenOptions::new();
	options
		.write(true)
		.create_new(true)
		.follow(FollowSymlinks::No);
	let mut file = module
		.open_with(MODULE_CLAIM_LOCK_NAME, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_LOCK_NAME),
			source,
		})?;
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: display.join(MODULE_CLAIM_LOCK_NAME),
		source,
	})?;
	let written = file
		.write_all(&bytes)
		.and_then(|()| file.sync_all())
		.map_err(|source| SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_LOCK_NAME),
			source,
		});
	drop(file);
	if let Err(error) = written {
		clear_module_claim_name(module, display, MODULE_CLAIM_LOCK_NAME, identity)?;
		return Err(error);
	}
	if let Err(source) = rename_noreplace_if_identity(
		module,
		OsStr::new(MODULE_CLAIM_LOCK_NAME),
		identity,
		module,
		OsStr::new(MODULE_CLAIM_NAME),
	) {
		clear_module_claim_name(module, display, MODULE_CLAIM_LOCK_NAME, identity)?;
		return Err(SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_NAME),
			source,
		});
	}
	sync_directory(module, display)?;
	ensure_named_identity(module, OsStr::new(MODULE_CLAIM_NAME), identity)?;
	Ok(identity)
}

fn clear_unpublished_module_claim(module: &Dir, display: &Path) -> Result<(), SubmoduleError> {
	match module.symlink_metadata(MODULE_CLAIM_LOCK_NAME) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
			let identity = EntryIdentity::from_metadata(&metadata);
			clear_module_claim_name(module, display, MODULE_CLAIM_LOCK_NAME, identity)
		}
		Ok(_) => Err(SubmoduleError::RecoveryRequired(
			"submodule set-url participant claim staging path is not a regular file".to_owned(),
		)),
		Err(source) => Err(SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_LOCK_NAME),
			source,
		}),
	}
}

fn clear_module_claim_name(
	module: &Dir,
	display: &Path,
	name: &str,
	expected: EntryIdentity,
) -> Result<(), SubmoduleError> {
	gitana_fs_native::remove_file_if_identity(module, OsStr::new(name), expected).map_err(
		|source| SubmoduleError::Io {
			path: display.join(name),
			source,
		},
	)?;
	sync_directory(module, display)
}

fn read_module_claim(
	module: &Dir,
	display: &Path,
) -> Result<Option<(SetUrlModuleClaimBody, EntryIdentity)>, SubmoduleError> {
	match module.symlink_metadata(MODULE_CLAIM_NAME) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule set-url participant claim is not a regular file".to_owned(),
			));
		}
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: display.join(MODULE_CLAIM_NAME),
				source,
			});
		}
	}
	let mut options = OpenOptions::new();
	options.read(true).follow(FollowSymlinks::No);
	let mut file = module
		.open_with(MODULE_CLAIM_NAME, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_NAME),
			source,
		})?;
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: display.join(MODULE_CLAIM_NAME),
		source,
	})?;
	let mut bytes = Vec::new();
	file
		.read_to_end(&mut bytes)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_NAME),
			source,
		})?;
	let claim = serde_json::from_slice(&bytes).map_err(|error| {
		SubmoduleError::RecoveryRequired(format!("invalid set-url module claim: {error}"))
	})?;
	ensure_named_identity(module, OsStr::new(MODULE_CLAIM_NAME), identity)?;
	Ok(Some((claim, identity)))
}

fn clear_orphaned_set_url_module_claim(
	module: &Dir,
	display: &Path,
	owner_identity: EntryIdentity,
	module_identity: EntryIdentity,
	declaration: &SubmoduleDeclaration,
) -> Result<(), SubmoduleError> {
	let Some((claim, identity)) = read_module_claim(module, display)? else {
		return Ok(());
	};
	if claim.version != INTENT_VERSION
		|| EntryIdentity::from(claim.owner_identity) != owner_identity
		|| EntryIdentity::from(claim.module_identity) != module_identity
		|| claim.name != declaration.name
		|| claim.path != declaration.path
	{
		return Err(SubmoduleError::RecoveryRequired(
			"another submodule set-url participant claim is pending".to_owned(),
		));
	}
	gitana_fs_native::remove_file_if_identity(module, OsStr::new(MODULE_CLAIM_NAME), identity)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(MODULE_CLAIM_NAME),
			source,
		})?;
	sync_directory(module, display)
}

/// Report whether a pending set-URL transaction owns a transient config-publication gap.
pub async fn pending_set_url_configs_require_restore<C: SetUrlConfigurationProvider>(
	git: Dir,
	git_dir: &Path,
	common: Dir,
	common_dir: &Path,
	work: Option<(Dir, PathBuf)>,
	configuration: &C,
) -> Result<bool, SubmoduleError> {
	let Some((intent, _)) = read_set_url_intent_at(&git, git_dir)? else {
		return Ok(false);
	};
	validate_set_url_intent(&intent)?;
	if intent.phase < SetUrlPhase::Ready {
		return Ok(false);
	}
	let mut required = false;
	if let Some(publication) = &intent.declaration.publication
		&& let Some((work, worktree_root)) = work
	{
		required |= configuration
			.set_url_before_image_requires_restore(
				work,
				Path::new(".gitmodules"),
				&worktree_root.join(".gitmodules"),
				&intent.declaration.transition,
				publication,
			)
			.await?;
	}
	if let Some(config) = &intent.superproject
		&& let Some(publication) = &config.publication
	{
		required |= configuration
			.set_url_before_image_requires_restore(
				common,
				Path::new("config"),
				&common_dir.join("config"),
				&config.transition,
				publication,
			)
			.await?;
	}
	if let Some(config) = &intent.module
		&& let Some(publication) = &config.publication
	{
		let relative = Path::new("modules").join(&intent.name);
		let display = git_dir.join(&relative);
		let module = open_subdir_nofollow(&git, &relative).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		if directory_identity(&module).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})? != EntryIdentity::from(intent.module_identity.ok_or_else(|| {
			SubmoduleError::RecoveryRequired("set-url module identity is missing".to_owned())
		})?) {
			return Err(SubmoduleError::RecoveryRequired(
				"module repository changed while inspecting set-url recovery".to_owned(),
			));
		}
		validate_recorded_module_claim(&intent, &git, git_dir, &module, &display)?;
		required |= configuration
			.set_url_before_image_requires_restore(
				module,
				Path::new("config"),
				&display.join("config"),
				&config.transition,
				publication,
			)
			.await?;
	}
	Ok(required)
}

/// Restore config before-images needed by ordinary command setup.
///
/// This repairs only native publication gaps; the set-URL operation remains the sole authority
/// for abandoning or completing the semantic transaction.
pub async fn restore_pending_set_url_configs<C: SetUrlConfigurationProvider>(
	git: Dir,
	git_dir: &Path,
	common: Dir,
	common_dir: &Path,
	work: Option<(Dir, PathBuf)>,
	configuration: &C,
) -> Result<(), SubmoduleError> {
	let Some((intent, intent_identity)) = read_set_url_intent_at(&git, git_dir)? else {
		return Ok(());
	};
	validate_set_url_intent(&intent)?;
	if intent.phase < SetUrlPhase::Ready {
		return Ok(());
	}
	let lock = acquire_update_lock_with_common(&git, git_dir, &common, common_dir)?;
	lock.validate()?;
	ensure_active_set_url_intent(&git, git_dir, intent_identity)?;
	if let Some(publication) = &intent.declaration.publication
		&& let Some((work, worktree_root)) = work
	{
		configuration
			.restore_set_url_before_image(
				work,
				Path::new(".gitmodules"),
				&worktree_root.join(".gitmodules"),
				&intent.declaration.transition,
				publication,
				lock.lease(),
			)
			.await?;
	}
	if let Some(config) = &intent.superproject
		&& let Some(publication) = &config.publication
	{
		ensure_active_set_url_intent(&git, git_dir, intent_identity)?;
		configuration
			.restore_set_url_before_image(
				common.try_clone().map_err(|source| SubmoduleError::Io {
					path: common_dir.to_owned(),
					source,
				})?,
				Path::new("config"),
				&common_dir.join("config"),
				&config.transition,
				publication,
				lock.lease(),
			)
			.await?;
	}
	if let Some(config) = &intent.module
		&& let Some(publication) = &config.publication
	{
		ensure_active_set_url_intent(&git, git_dir, intent_identity)?;
		let relative = Path::new("modules").join(&intent.name);
		let display = git_dir.join(&relative);
		let module = open_subdir_nofollow(&git, &relative).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})?;
		if directory_identity(&module).map_err(|source| SubmoduleError::Io {
			path: display.clone(),
			source,
		})? != EntryIdentity::from(intent.module_identity.ok_or_else(|| {
			SubmoduleError::RecoveryRequired("set-url module identity is missing".to_owned())
		})?) {
			return Err(SubmoduleError::RecoveryRequired(
				"module repository changed while restoring set-url recovery".to_owned(),
			));
		}
		validate_recorded_module_claim(&intent, &git, git_dir, &module, &display)?;
		let module_lease = try_acquire_submodule_config_mutation_lease(&module, &display)?;
		configuration
			.restore_set_url_before_image(
				module,
				Path::new("config"),
				&display.join("config"),
				&config.transition,
				publication,
				lock.lease().combine(module_lease),
			)
			.await?;
	}
	lock.validate()
}

fn ensure_active_set_url_intent(
	git: &Dir,
	git_dir: &Path,
	expected: EntryIdentity,
) -> Result<(), SubmoduleError> {
	let control =
		open_subdir_nofollow(git, Path::new(CONTROL_DIR)).map_err(|source| SubmoduleError::Io {
			path: git_dir.join(CONTROL_DIR),
			source,
		})?;
	ensure_named_identity(&control, OsStr::new(INTENT_NAME), expected)
}

fn ensure_distinct_set_url_targets(intent: &SetUrlIntent) -> Result<(), SubmoduleError> {
	let mut targets = vec![(".gitmodules", &intent.declaration.transition)];
	if let Some(config) = &intent.superproject {
		targets.push(("superproject", &config.transition));
	}
	if let Some(config) = &intent.module {
		targets.push(("module", &config.transition));
	}
	for (index, (left_name, left)) in targets.iter().enumerate() {
		let Some(left_identity) = left.target.target() else {
			continue;
		};
		for (right_name, right) in &targets[index + 1..] {
			if right.target.target() == Some(left_identity) {
				return Err(SubmoduleError::Configuration(format!(
					"set-url config targets for {left_name} and {right_name} resolve to the same file"
				)));
			}
		}
	}
	Ok(())
}

fn validate_set_url_intent(intent: &SetUrlIntent) -> Result<(), SubmoduleError> {
	if intent.version != INTENT_VERSION {
		return Err(SubmoduleError::RecoveryRequired(format!(
			"unsupported set-url intent version {}",
			intent.version
		)));
	}
	validate_name(&intent.name)?;
	validate_path(&intent.path)?;
	if intent.transaction.is_empty() {
		return Err(SubmoduleError::RecoveryRequired(
			"set-url transaction identifier is empty".to_owned(),
		));
	}
	validate_set_url_config(&intent.declaration, intent.phase >= SetUrlPhase::Ready)?;
	if let Some(config) = &intent.superproject {
		validate_set_url_config(config, intent.phase >= SetUrlPhase::Ready)?;
	}
	if let Some(config) = &intent.module {
		validate_set_url_config(config, intent.phase >= SetUrlPhase::Ready)?;
		if intent.module_remote.is_none()
			|| intent.module_identity.is_none()
			|| intent.mount_identity.is_none()
			|| intent.mount_marker.is_none()
			|| intent.module_claim.is_none()
		{
			return Err(SubmoduleError::RecoveryRequired(
				"set-url module recovery proof is incomplete".to_owned(),
			));
		}
	} else if intent.module_remote.is_some()
		|| intent.module_identity.is_some()
		|| intent.mount_identity.is_some()
		|| intent.mount_marker.is_some()
		|| intent.module_claim.is_some()
	{
		return Err(SubmoduleError::RecoveryRequired(
			"set-url intent records module proofs without a module transition".to_owned(),
		));
	}
	if let Some(claim) = &intent.module_claim
		&& (claim.version != INTENT_VERSION
			|| claim.transaction != intent.transaction
			|| claim.name != intent.name
			|| claim.path != intent.path
			|| Some(claim.module_identity) != intent.module_identity)
	{
		return Err(SubmoduleError::RecoveryRequired(
			"set-url module claim does not match its coordinator".to_owned(),
		));
	}
	ensure_distinct_set_url_targets(intent)
}

fn validate_set_url_config(config: &SetUrlConfig, ready: bool) -> Result<(), SubmoduleError> {
	if !config.transition.changes() {
		if config.publication.is_some() {
			return Err(SubmoduleError::RecoveryRequired(
				"unchanged set-url transition records a publication".to_owned(),
			));
		}
		return Ok(());
	}
	match &config.publication {
		Some(publication) if private_config_component(&publication.name) => Ok(()),
		Some(_) => Err(SubmoduleError::RecoveryRequired(
			"set-url publication name is unsafe".to_owned(),
		)),
		None if ready => Err(SubmoduleError::RecoveryRequired(
			"ready set-url transition has no publication".to_owned(),
		)),
		None => Ok(()),
	}
}

fn read_set_url_intent_at(
	git: &Dir,
	git_dir: &Path,
) -> Result<Option<(SetUrlIntent, EntryIdentity)>, SubmoduleError> {
	let control = match open_subdir_nofollow(git, Path::new(CONTROL_DIR)) {
		Ok(control) => control,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: git_dir.join(CONTROL_DIR),
				source,
			});
		}
	};
	read_intent(&control, &git_dir.join(CONTROL_DIR).join(INTENT_NAME))
}

fn read_intent(
	control: &Dir,
	display: &Path,
) -> Result<Option<(SetUrlIntent, EntryIdentity)>, SubmoduleError> {
	#[cfg(windows)]
	return read_windows_intent(control, display);
	#[cfg(not(windows))]
	read_named_intent(control, OsStr::new(INTENT_NAME), display)
}

#[cfg(windows)]
fn read_windows_intent(
	control: &Dir,
	display: &Path,
) -> Result<Option<(SetUrlIntent, EntryIdentity)>, SubmoduleError> {
	let current = read_named_intent(control, OsStr::new(INTENT_NAME), display)?;
	let previous_display = display.with_file_name(INTENT_PREVIOUS_NAME);
	let previous = read_named_intent(control, OsStr::new(INTENT_PREVIOUS_NAME), &previous_display)?;
	match (current, previous) {
		(Some(current), Some(previous)) => {
			ensure_named_identity(control, OsStr::new(INTENT_NAME), current.1)?;
			ensure_named_identity(control, OsStr::new(INTENT_PREVIOUS_NAME), previous.1)?;
			if !same_set_url_transaction(&previous.0, &current.0) || previous.0.phase > current.0.phase {
				return Err(SubmoduleError::RecoveryRequired(
					"set-url intent predecessor does not match the active transaction".to_owned(),
				));
			}
			sync_directory(control, display.parent().unwrap_or(Path::new("")))?;
			Ok(Some(current))
		}
		(Some(current), None) => {
			ensure_named_identity(control, OsStr::new(INTENT_NAME), current.1)?;
			if identity_if_present(control, OsStr::new(INTENT_PREVIOUS_NAME))?.is_some() {
				return Err(SubmoduleError::RecoveryRequired(
					"set-url intent predecessor appeared while the journal was being read".to_owned(),
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

#[cfg(windows)]
fn promote_windows_intent_predecessor(
	control: &Dir,
	display: &Path,
	intent: SetUrlIntent,
	identity: EntryIdentity,
) -> Result<(SetUrlIntent, EntryIdentity), SubmoduleError> {
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
			let current = read_named_intent(control, OsStr::new(INTENT_NAME), display)?;
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

#[cfg(windows)]
fn finish_windows_intent_predecessor_promotion(
	control: &Dir,
	display: &Path,
	identity: EntryIdentity,
) -> Result<(), SubmoduleError> {
	sync_directory(control, display.parent().unwrap_or(Path::new("")))?;
	ensure_named_identity(control, OsStr::new(INTENT_NAME), identity)?;
	if identity_if_present(control, OsStr::new(INTENT_PREVIOUS_NAME))?.is_some() {
		return Err(SubmoduleError::RecoveryRequired(
			"set-url intent predecessor appeared while its promotion was being recovered".to_owned(),
		));
	}
	Ok(())
}

fn read_named_intent(
	control: &Dir,
	name: &OsStr,
	display: &Path,
) -> Result<Option<(SetUrlIntent, EntryIdentity)>, SubmoduleError> {
	match control.symlink_metadata(name) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"set-url intent is not a regular file".to_owned(),
			));
		}
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
	let intent = serde_json::from_slice(&bytes).map_err(|error| {
		SubmoduleError::RecoveryRequired(format!("invalid set-url intent: {error}"))
	})?;
	ensure_named_identity(control, name, identity)?;
	Ok(Some((intent, identity)))
}

fn write_new_intent(
	control: &Dir,
	display: &Path,
	intent: &SetUrlIntent,
) -> Result<EntryIdentity, SubmoduleError> {
	let bytes = serde_json::to_vec(intent).map_err(|error| {
		SubmoduleError::RecoveryRequired(format!("serializing set-url intent: {error}"))
	})?;
	let mut options = OpenOptions::new();
	options
		.write(true)
		.create_new(true)
		.follow(FollowSymlinks::No);
	let mut file = control
		.open_with(INTENT_LOCK_NAME, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(INTENT_LOCK_NAME),
			source,
		})?;
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: display.join(INTENT_LOCK_NAME),
		source,
	})?;
	file
		.write_all(&bytes)
		.and_then(|()| file.sync_all())
		.map_err(|source| SubmoduleError::Io {
			path: display.join(INTENT_LOCK_NAME),
			source,
		})?;
	drop(file);
	rename_noreplace_if_identity(
		control,
		OsStr::new(INTENT_LOCK_NAME),
		identity,
		control,
		OsStr::new(INTENT_NAME),
	)
	.map_err(|source| SubmoduleError::Io {
		path: display.join(INTENT_NAME),
		source,
	})?;
	sync_directory(control, display)?;
	Ok(identity)
}

fn replace_intent(
	control: &Dir,
	display: &Path,
	intent: &SetUrlIntent,
	expected: EntryIdentity,
) -> Result<EntryIdentity, SubmoduleError> {
	let bytes = serde_json::to_vec(intent).map_err(|error| {
		SubmoduleError::RecoveryRequired(format!("serializing set-url intent: {error}"))
	})?;
	let name = private_intent_name(control, display)?;
	let mut options = OpenOptions::new();
	options
		.write(true)
		.create_new(true)
		.follow(FollowSymlinks::No);
	let mut file = control
		.open_with(&name, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(&name),
			source,
		})?;
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: display.join(&name),
		source,
	})?;
	file
		.write_all(&bytes)
		.and_then(|()| file.sync_all())
		.map_err(|source| SubmoduleError::Io {
			path: display.join(&name),
			source,
		})?;
	drop(file);
	#[cfg(not(windows))]
	{
		replace_if_identities(control, &name, identity, OsStr::new(INTENT_NAME), expected).map_err(
			|source| SubmoduleError::Io {
				path: display.join(INTENT_NAME),
				source,
			},
		)?;
		sync_directory(control, display)?;
	}
	#[cfg(windows)]
	{
		if let Some((previous, previous_identity)) = read_named_intent(
			control,
			OsStr::new(INTENT_PREVIOUS_NAME),
			&display.join(INTENT_PREVIOUS_NAME),
		)? {
			if !same_set_url_transaction(&previous, intent) || previous.phase > intent.phase {
				return Err(SubmoduleError::RecoveryRequired(
					"set-url intent predecessor does not match the active transaction".to_owned(),
				));
			}
			remove_file_if_identity(control, OsStr::new(INTENT_PREVIOUS_NAME), previous_identity)
				.map_err(|source| SubmoduleError::Io {
					path: display.join(INTENT_PREVIOUS_NAME),
					source,
				})?;
			sync_directory(control, display)?;
		}
		rename_noreplace_if_identity(
			control,
			OsStr::new(INTENT_NAME),
			expected,
			control,
			OsStr::new(INTENT_PREVIOUS_NAME),
		)
		.map_err(|source| SubmoduleError::Io {
			path: display.join(INTENT_PREVIOUS_NAME),
			source,
		})?;
		sync_directory(control, display)?;
		rename_noreplace_if_identity(control, &name, identity, control, OsStr::new(INTENT_NAME))
			.map_err(|source| SubmoduleError::Io {
				path: display.join(INTENT_NAME),
				source,
			})?;
		sync_directory(control, display)?;
		remove_file_if_identity(control, OsStr::new(INTENT_PREVIOUS_NAME), expected).map_err(
			|source| SubmoduleError::Io {
				path: display.join(INTENT_PREVIOUS_NAME),
				source,
			},
		)?;
		sync_directory(control, display)?;
	}
	Ok(identity)
}

fn private_intent_name(control: &Dir, display: &Path) -> Result<OsString, SubmoduleError> {
	for _ in 0..PRIVATE_ATTEMPTS {
		let sequence = PRIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
		let name = OsString::from(format!(
			".gitana-submodule-set-url-intent.{}.{}",
			std::process::id(),
			sequence
		));
		match control.symlink_metadata(&name) {
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
			Ok(_) => {}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: display.join(name),
					source,
				});
			}
		}
	}
	Err(SubmoduleError::RecoveryRequired(
		"could not reserve a private set-url journal update".to_owned(),
	))
}

fn ensure_named_identity(
	directory: &Dir,
	name: &OsStr,
	expected: EntryIdentity,
) -> Result<(), SubmoduleError> {
	if entry_identity(directory, name).map_err(|source| SubmoduleError::Io {
		path: PathBuf::from(name),
		source,
	})? != expected
	{
		return Err(SubmoduleError::RecoveryRequired(
			"active set-url intent changed during recovery".to_owned(),
		));
	}
	Ok(())
}

#[cfg(windows)]
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

fn open_subdir_nofollow(directory: &Dir, relative: &Path) -> std::io::Result<Dir> {
	let mut current = directory.try_clone()?;
	for component in relative.components() {
		let Component::Normal(component) = component else {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"unsafe set-url control path",
			));
		};
		let metadata = current.symlink_metadata(component)?;
		if metadata.file_type().is_symlink() || !metadata.is_dir() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"set-url control component is not a directory",
			));
		}
		current = current.open_dir_nofollow(component)?;
	}
	Ok(current)
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

#[cfg(windows)]
fn same_set_url_transaction(previous: &SetUrlIntent, current: &SetUrlIntent) -> bool {
	previous.version == current.version
		&& previous.transaction == current.transaction
		&& previous.name == current.name
		&& previous.path == current.path
		&& preserves_config(&previous.declaration, &current.declaration)
		&& preserves_optional_config(&previous.superproject, &current.superproject)
		&& preserves_optional_config(&previous.module, &current.module)
		&& previous.module_remote == current.module_remote
		&& previous.module_identity == current.module_identity
		&& previous.mount_identity == current.mount_identity
		&& previous.mount_marker == current.mount_marker
		&& previous.module_claim == current.module_claim
}

#[cfg(windows)]
fn preserves_config(previous: &SetUrlConfig, current: &SetUrlConfig) -> bool {
	previous.transition == current.transition
		&& previous
			.publication
			.as_ref()
			.is_none_or(|publication| current.publication.as_ref() == Some(publication))
}

#[cfg(windows)]
fn preserves_optional_config(
	previous: &Option<SetUrlConfig>,
	current: &Option<SetUrlConfig>,
) -> bool {
	match (previous, current) {
		(None, _) => true,
		(Some(previous), Some(current)) => preserves_config(previous, current),
		(Some(_), None) => false,
	}
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

#[cfg(test)]
mod tests {
	use cap_std::ambient_authority;

	use super::*;

	#[test]
	fn module_claim_blocks_mutation_and_an_owned_orphan_is_retired() {
		let temporary = tempfile::tempdir().unwrap();
		let owner_path = temporary.path().join("owner");
		let module_path = owner_path.join("modules/one");
		std::fs::create_dir_all(&module_path).unwrap();
		let owner = Dir::open_ambient_dir(&owner_path, ambient_authority()).unwrap();
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let owner_identity = directory_identity(&owner).unwrap();
		let module_identity = directory_identity(&module).unwrap();
		let declaration = SubmoduleDeclaration {
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			url: Some("next".to_owned()),
			branch: None,
			update: None,
			shallow: None,
		};
		let claim = SetUrlModuleClaimBody {
			version: INTENT_VERSION,
			transaction: "transaction".to_owned(),
			owner_identity: owner_identity.into(),
			module_identity: module_identity.into(),
			name: declaration.name.clone(),
			path: declaration.path.clone(),
		};
		write_new_module_claim(&module, &module_path, &claim).unwrap();

		assert!(repository_has_pending_set_url(&module, &module_path).unwrap());
		assert!(repository_has_set_url_participant_claim(&module, &module_path).unwrap());
		clear_orphaned_set_url_module_claim(
			&module,
			&module_path,
			owner_identity,
			module_identity,
			&declaration,
		)
		.unwrap();
		assert!(!repository_has_pending_set_url(&module, &module_path).unwrap());
		assert!(!repository_has_set_url_participant_claim(&module, &module_path).unwrap());
	}

	#[test]
	fn module_claim_replaces_only_its_private_incomplete_staging_file() {
		let temporary = tempfile::tempdir().unwrap();
		let module_path = temporary.path().join("module");
		std::fs::create_dir(&module_path).unwrap();
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let module_identity = directory_identity(&module).unwrap();
		let claim = SetUrlModuleClaimBody {
			version: INTENT_VERSION,
			transaction: "first".to_owned(),
			owner_identity: EntryIdentity::from_parts(1, 2).into(),
			module_identity: module_identity.into(),
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
		};
		std::fs::write(module_path.join(MODULE_CLAIM_LOCK_NAME), b"{partial").unwrap();

		let identity = write_new_module_claim(&module, &module_path, &claim).unwrap();
		assert!(!module_path.join(MODULE_CLAIM_LOCK_NAME).exists());
		assert_eq!(
			read_module_claim(&module, &module_path).unwrap(),
			Some((claim.clone(), identity))
		);

		std::fs::write(module_path.join(MODULE_CLAIM_LOCK_NAME), b"{partial-again").unwrap();
		let replacement = SetUrlModuleClaimBody {
			transaction: "second".to_owned(),
			..claim.clone()
		};
		assert!(write_new_module_claim(&module, &module_path, &replacement).is_err());
		assert!(!module_path.join(MODULE_CLAIM_LOCK_NAME).exists());
		assert_eq!(
			read_module_claim(&module, &module_path).unwrap(),
			Some((claim, identity))
		);
	}

	#[test]
	fn module_claim_for_another_coordinator_is_preserved() {
		let temporary = tempfile::tempdir().unwrap();
		let module_path = temporary.path().join("module");
		std::fs::create_dir(&module_path).unwrap();
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let module_identity = directory_identity(&module).unwrap();
		let declaration = SubmoduleDeclaration {
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			url: Some("next".to_owned()),
			branch: None,
			update: None,
			shallow: None,
		};
		let claim = SetUrlModuleClaimBody {
			version: INTENT_VERSION,
			transaction: "transaction".to_owned(),
			owner_identity: EntryIdentity::from_parts(1, 2).into(),
			module_identity: module_identity.into(),
			name: declaration.name.clone(),
			path: declaration.path.clone(),
		};
		write_new_module_claim(&module, &module_path, &claim).unwrap();

		assert!(matches!(
			clear_orphaned_set_url_module_claim(
				&module,
				&module_path,
				EntryIdentity::from_parts(3, 4),
				module_identity,
				&declaration,
			),
			Err(SubmoduleError::RecoveryRequired(_))
		));
		assert!(repository_has_pending_set_url(&module, &module_path).unwrap());
	}

	#[test]
	fn coordinator_control_is_not_a_module_participant_claim() {
		let temporary = tempfile::tempdir().unwrap();
		std::fs::create_dir(temporary.path().join(CONTROL_DIR)).unwrap();
		let git = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

		assert!(repository_has_pending_set_url(&git, temporary.path()).unwrap());
		assert!(!repository_has_set_url_participant_claim(&git, temporary.path()).unwrap());
	}

	#[test]
	fn repository_recovery_includes_linked_worktree_coordinators() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("module");
		let linked_path = common_path.join("worktrees/linked");
		std::fs::create_dir_all(&linked_path).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let layout = RepositoryLayout {
			worktree_root: None,
			git_dir: common_path.clone(),
			common_dir: common_path.clone(),
		};

		assert!(!repository_has_pending_set_url_recovery(&common, &common, &layout).unwrap());
		std::fs::create_dir(linked_path.join(CONTROL_DIR)).unwrap();
		assert!(repository_has_pending_set_url_recovery(&common, &common, &layout).unwrap());
	}
}
