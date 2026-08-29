use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, OpenOptions};
use gitana_file_store::{DurabilityTarget, FileStore};
use gitana_file_store_local::{CapWorkDir, LocalFileStore, WorkDirFs, same_directory_identity};
use gitana_fs_native::{
	EntryIdentity, directory_identity, file_identity, remove_dir_all_if_identity,
	remove_file_if_identity, rename_noreplace, rename_noreplace_if_identity, replace_if_identities,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::{ReflogIntent, Repository, detect_hash_kind};
use gitana_worktree::WorkTree;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Digest};

use crate::context::{
	declarations_by_path, is_active, parse_marker_target, validate_update_strategy,
};
use crate::{
	ConfigurationProvider, FetchRepository, FetchSource, InitRequest, PrepareRepository,
	PrepareSource, RepositoryTransfer, SubmoduleContext, SubmoduleDeclaration, SubmoduleError,
	SubmoduleObjectId, UpdateFailure, UpdateOutcome, UpdateOutcomeState, UpdateReport, UpdateRequest,
};

const CONTROL_DIR: &str = "gitana-submodule-update";
const INTENT_FILE: &str = "gitana-submodule-update/intent.json";
const INTENT_LOCK: &str = "gitana-submodule-update/intent.lock";
const STAGED_REPOSITORY: &str = "gitana-submodule-update/repository";
const INTENT_NAME: &str = "intent.json";
const INTENT_LOCK_NAME: &str = "intent.lock";
const STAGED_REPOSITORY_NAME: &str = "repository";
const UPDATE_LOCK: &str = "gitana-submodule-update.lock";
const MARKER_TEMP_ATTEMPTS: u64 = 100;
static MARKER_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static CONTROL_RETIRE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Planned<H: HashAlgorithm> {
	declaration: SubmoduleDeclaration,
	recorded: ObjectId<H>,
	source_url: Option<String>,
	state: Option<UpdateOutcomeState>,
	recovering: bool,
	intent_identity: Option<EntryIdentity>,
	pointers: ModulePointers,
}

#[derive(Clone, Copy)]
enum DirectoryNamespace<'a> {
	Module(&'a Path),
	Mount(&'a str),
}

impl DirectoryNamespace<'_> {
	fn unsafe_component(self) -> SubmoduleError {
		match self {
			Self::Module(target) => SubmoduleError::UnsafeName(target.display().to_string()),
			Self::Mount(path) => SubmoduleError::UnsafePath(path.to_owned()),
		}
	}

	fn occupied(self) -> SubmoduleError {
		match self {
			Self::Module(target) => SubmoduleError::InvalidRepository(target.display().to_string()),
			Self::Mount(path) => SubmoduleError::ForeignMount(path.to_owned()),
		}
	}
}

struct ModulePointers {
	core_worktree: String,
	marker: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerIdentity {
	device: u64,
	inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MarkerSnapshot {
	Absent,
	File {
		identity: MarkerIdentity,
		bytes: Vec<u8>,
	},
}

struct MountPlan {
	directory: Dir,
	mounted: bool,
	newly_attached: bool,
	marker: MarkerSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IntentSourceContext {
	Superproject,
	Module,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StageIntent {
	version: u32,
	name: String,
	path: String,
	recorded: String,
	source_fingerprint: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	source_context: Option<IntentSourceContext>,
}

struct ConditionalFileCleanup {
	directory: Dir,
	name: &'static str,
	identity: EntryIdentity,
	armed: bool,
}

/// A per-worktree update lease tied to both the retained Git directory and the named lock entry.
///
/// The directory lock prevents a second Gitana invocation from entering the shared staging
/// namespace if an unrelated process detaches the named lock on Unix. The entry identity still
/// detects that namespace tampering so the current operation fails closed at its next boundary.
struct UpdateLockGuard {
	directory: Dir,
	identity: EntryIdentity,
	display_path: PathBuf,
	_directory_lock: Option<File>,
	_named_lock: File,
}

impl UpdateLockGuard {
	fn validate(&self) -> Result<(), SubmoduleError> {
		let metadata = self
			.directory
			.symlink_metadata(UPDATE_LOCK)
			.map_err(|source| {
				if source.kind() == std::io::ErrorKind::NotFound {
					SubmoduleError::RecoveryRequired(
						"submodule update lock entry changed while held".to_owned(),
					)
				} else {
					SubmoduleError::Io {
						path: self.display_path.clone(),
						source,
					}
				}
			})?;
		if !metadata.is_file()
			|| metadata.file_type().is_symlink()
			|| EntryIdentity::from_metadata(&metadata) != self.identity
		{
			return Err(SubmoduleError::RecoveryRequired(
				"submodule update lock entry changed while held".to_owned(),
			));
		}
		Ok(())
	}
}

impl Drop for ConditionalFileCleanup {
	fn drop(&mut self) {
		if self.armed {
			let _ = remove_file_if_identity(&self.directory, OsStr::new(self.name), self.identity);
		}
	}
}

impl SubmoduleContext {
	pub async fn update<C: ConfigurationProvider, T: RepositoryTransfer>(
		&self,
		request: &UpdateRequest,
		configuration: &C,
		transfer: &T,
	) -> Result<UpdateReport, UpdateFailure> {
		match self.hash_kind {
			HashKind::Sha1 => {
				self
					.update_typed::<Sha1, C, T>(request, configuration, transfer)
					.await
			}
			HashKind::Sha256 => {
				self
					.update_typed::<Sha256, C, T>(request, configuration, transfer)
					.await
			}
		}
	}

	async fn update_typed<H: HashAlgorithm, C: ConfigurationProvider, T: RepositoryTransfer>(
		&self,
		request: &UpdateRequest,
		configuration: &C,
		transfer: &T,
	) -> Result<UpdateReport, UpdateFailure> {
		let worktree = self.worktree::<H>().map_err(UpdateFailure::preflight)?;
		let index = worktree
			.load_index()
			.await
			.map_err(SubmoduleError::from)
			.map_err(UpdateFailure::preflight)?;
		let selected = self
			.select_gitlinks(&index, &request.query)
			.map_err(UpdateFailure::preflight)?;
		let declarations = declarations_by_path(
			self
				.declarations()
				.await
				.map_err(UpdateFailure::preflight)?,
		)
		.map_err(UpdateFailure::preflight)?;

		// Structural preflight is deliberately complete before `--init` or filesystem mutation.
		let mut pointers = HashMap::with_capacity(selected.len());
		for path in &selected {
			let declaration = declarations
				.get(path)
				.ok_or_else(|| SubmoduleError::MissingMapping(path.clone()))
				.map_err(UpdateFailure::preflight)?;
			if index.conflict(path).is_some() {
				return Err(UpdateFailure::preflight(SubmoduleError::Conflicted(
					path.clone(),
				)));
			}
			if let Some(strategy) = declaration.update.as_deref() {
				validate_update_strategy(&declaration.name, strategy).map_err(UpdateFailure::preflight)?;
			}
			let strategy = configured_update_strategy(&self.configs.superproject, &declaration.name)
				.map_err(UpdateFailure::preflight)?
				.or_else(|| declaration.update.clone())
				.unwrap_or_else(|| "checkout".to_owned());
			validate_update_strategy(&declaration.name, &strategy).map_err(UpdateFailure::preflight)?;
			let module_pointers = self
				.module_pointers(declaration)
				.map_err(UpdateFailure::preflight)?;
			self
				.preflight_module_namespaces(declaration, &module_pointers)
				.map_err(UpdateFailure::preflight)?;
			pointers.insert(path.clone(), module_pointers);
		}

		let initialized = if request.initialize {
			self
				.init(
					&InitRequest {
						query: request.query.clone(),
					},
					configuration,
				)
				.await
				.map_err(UpdateFailure::preflight)?
		} else {
			crate::InitReport::default()
		};
		let mut report = UpdateReport {
			initialization: initialized,
			outcomes: Vec::new(),
		};
		let credential_urls: HashMap<String, String> = report
			.initialization
			.outcomes
			.iter()
			.filter_map(|outcome| {
				outcome
					.credential_url
					.clone()
					.map(|url| (outcome.path.clone(), url))
			})
			.collect();
		let effective = configuration
			.reload()
			.await
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		let mut plan = Vec::with_capacity(selected.len());
		for path in selected {
			let declaration = declarations
				.get(&path)
				.expect("preflight established every mapping")
				.clone();
			let recorded = index.entry(&path).expect("selected stage-zero gitlink").oid;
			let configured_url = effective.get_raw("submodule", Some(&declaration.name), "url");
			let source_url = match configured_url {
				Some(Some(url)) => Some(url.to_owned()),
				Some(None) => {
					return Err(UpdateFailure::after_init(
						&report,
						SubmoduleError::MissingValue(format!("submodule.{}.url", declaration.name)),
					));
				}
				None => None,
			};
			let source_url = credential_urls.get(&path).cloned().or(source_url);
			let active = is_active(&effective, &declaration.name, &declaration.path)
				.map_err(|source| UpdateFailure::after_init(&report, source))?;
			let strategy = configured_update_strategy(&effective, &declaration.name)
				.map_err(|source| UpdateFailure::after_init(&report, source))?
				.or_else(|| declaration.update.clone())
				.unwrap_or_else(|| "checkout".to_owned());
			validate_update_strategy(&declaration.name, &strategy)
				.map_err(|source| UpdateFailure::after_init(&report, source))?;
			let state = if source_url.is_none() {
				Some(UpdateOutcomeState::SkippedUnregistered)
			} else if !active {
				Some(UpdateOutcomeState::SkippedInactive)
			} else if strategy == "none" {
				Some(UpdateOutcomeState::SkippedByStrategy)
			} else {
				None
			};
			plan.push(Planned {
				declaration,
				recorded,
				source_url,
				state,
				recovering: false,
				intent_identity: None,
				pointers: pointers
					.remove(&path)
					.expect("preflight computed every selected module pointer"),
			});
		}

		let lock = self
			.acquire_update_lock()
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		lock
			.validate()
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		let recovery_index = self
			.recover_stage(&mut plan, configuration, transfer)
			.await
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		lock
			.validate()
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		if let Some(recovery_index) = recovery_index
			&& recovery_index != 0
		{
			// The durable intent exclusively owns the shared staging namespace. Complete its
			// module before an earlier index entry can attempt to prepare a different repository;
			// removing and reinserting preserves the relative index order of every other entry.
			let recovering = plan.remove(recovery_index);
			plan.insert(0, recovering);
		}
		for entry in plan {
			lock.validate().map_err(|source| UpdateFailure {
				completed: report.clone(),
				module: Some(entry.declaration.name.clone()),
				source,
			})?;
			if let Some(state) = entry.state {
				report.outcomes.push(outcome(&entry, state));
				continue;
			}
			let module = entry.declaration.name.clone();
			let update = self
				.update_one(
					&entry,
					request.reflog_committer.as_deref(),
					configuration,
					transfer,
				)
				.await;
			match update {
				Ok(state) => {
					report.outcomes.push(outcome(&entry, state));
					lock.validate().map_err(|source| UpdateFailure {
						completed: report.clone(),
						module: Some(module),
						source,
					})?;
				}
				Err(source) => {
					let source = lock.validate().err().unwrap_or(source);
					return Err(UpdateFailure {
						completed: report,
						module: Some(module),
						source,
					});
				}
			}
		}
		lock
			.validate()
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		Ok(report)
	}

	async fn update_one<H: HashAlgorithm, C: ConfigurationProvider, T: RepositoryTransfer>(
		&self,
		entry: &Planned<H>,
		committer: Option<&str>,
		configuration: &C,
		transfer: &T,
	) -> Result<UpdateOutcomeState, SubmoduleError> {
		let mut intent_identity = entry.intent_identity;
		let module_relative = Path::new("modules").join(&entry.declaration.name);
		let existing = match self.git.symlink_metadata(&module_relative) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
			Ok(_) => {
				return Err(SubmoduleError::InvalidRepository(
					entry.declaration.name.clone(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(&module_relative),
					source,
				});
			}
		};

		let mut cloned = false;
		if !existing {
			self.ensure_empty_mount(&entry.declaration.path)?;
		} else {
			self.ensure_mount_directory(&entry.declaration.path, false)?;
		}

		// Pin the mount before any transfer. The transfer can take an arbitrary amount of time, so
		// this snapshot must not be used for publication until both its namespace identity and its
		// marker/emptiness state have been revalidated.
		let mount_before_transfer = self.inspect_module_mount(entry).await?;
		if !existing {
			let source = entry
				.source_url
				.as_ref()
				.ok_or_else(|| SubmoduleError::Unregistered(entry.declaration.name.clone()))?;
			intent_identity = Some(self.prepare_module(entry, source, transfer).await?);
			cloned = true;
		}
		let completion_required = cloned || entry.recovering || mount_before_transfer.newly_attached;
		let module_git_dir = self.layout.git_dir.join(&module_relative);
		let (mut repository, module_directory) =
			self.open_module_repository::<H>(&entry.declaration)?;
		let hash_directory = module_directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: module_git_dir.clone(),
				source,
			})?;
		if configuration
			.module_hash_kind(hash_directory, &module_git_dir)
			.await?
			!= crate::object_id::kind::<H>()
		{
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		let config_directory = module_directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: module_git_dir.clone(),
				source,
			})?;
		let config = configuration
			.load_module_config(config_directory, &module_git_dir)
			.await?;
		// The native config loader may follow includes and supported config symlinks through ambient
		// read authority. Confirm that its repository path still names the retained module before the
		// resulting transport settings are allowed to drive a fetch into that capability.
		self.ensure_module_identity(entry, &module_directory)?;
		// The native frontend owns the complete system/global/local/command stack. Install that same
		// effective view on the repository before any checkout or HEAD publication so ref policy (for
		// example `core.logAllRefUpdates`) observes the invocation's actual configuration.
		repository.set_effective_config(config.clone());
		// A published repository accepted by `recover_stage` already has a durable recorded object
		// graph and is source-bound by its matching intent. Recovery must therefore finish from that
		// local state even if the original source has disappeared. Ordinary existing repositories keep
		// Git's fetch-first update behavior, including retained repositories without a recovery intent.
		let mut resolved_existing_source = None;
		if existing && !entry.recovering {
			let source = module_origin_url(&config, &entry.declaration.name)?;
			let transfer_directory =
				module_directory
					.try_clone()
					.map_err(|source| SubmoduleError::Io {
						path: module_git_dir.clone(),
						source,
					})?;
			let fetch_source = FetchSource {
				source_url: source,
				worktree_dir: self.worktree_root().join(&entry.declaration.path),
				config: config.clone(),
			};
			let resolved = transfer
				.resolve_fetch_source_identity(&fetch_source)
				.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
			let fetched = transfer
				.fetch_recorded(FetchRepository {
					source: fetch_source,
					git_dir: transfer_directory,
					display_git_dir: module_git_dir.clone(),
					hash_kind: crate::object_id::kind::<H>(),
					recorded: SubmoduleObjectId::from_typed(entry.recorded),
				})
				.await
				.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
			if fetched.resolved_source != resolved {
				return Err(SubmoduleError::Transfer(
					"submodule transfer source changed during fetch".to_owned(),
				));
			}
			resolved_existing_source = Some(fetched.resolved_source);
		}
		if !repository.objects().exists_object(&entry.recorded).await? {
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		let target_tree = repository.commit_tree(entry.recorded).await?;
		let head_lock = repository.refs().lock_head().await?;
		let current = repository.refs().resolve_head().await?;
		if !completion_required && mount_before_transfer.mounted && current.is_none() {
			drop(head_lock);
			return Err(SubmoduleError::UnbornModuleHead(
				entry.declaration.name.clone(),
			));
		}
		let needs_checkout = completion_required || current != Some(entry.recorded);
		if existing
			&& needs_checkout
			&& let Some(operation) = operation_in_progress(&repository).await?
		{
			drop(head_lock);
			return Err(SubmoduleError::OperationInProgress {
				name: entry.declaration.name.clone(),
				operation,
			});
		}
		let excludes_file = if needs_checkout {
			configuration
				.load_module_excludes(&config, &self.worktree_root().join(&entry.declaration.path))
				.await?
		} else {
			None
		};
		let mount = self
			.revalidate_module_mount(entry, &mount_before_transfer)
			.await?;
		let resolved_attachment_source = if mount.newly_attached && !cloned && !entry.recovering {
			Some(resolved_existing_source.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"retained mount publication requires the fetched source identity".to_owned(),
				)
			})?)
		} else {
			None
		};
		if !completion_required && mount.mounted && current == Some(entry.recorded) {
			let published_intent = self
				.publish_module_mount(
					entry,
					&mount,
					&module_directory,
					false,
					configuration,
					resolved_attachment_source.as_deref(),
				)
				.await?;
			debug_assert!(published_intent.is_none());
			self.ensure_module_identity(entry, &module_directory)?;
			self.ensure_mount_identity(entry, &mount)?;
			self.ensure_mount_marker(entry, &mount)?;
			drop(head_lock);
			return Ok(UpdateOutcomeState::AlreadyCurrent);
		}
		let message = format!(
			"checkout: moving from {} to {}",
			current.map_or_else(|| "unborn".to_owned(), |oid| oid.to_hex()),
			entry.recorded
		);
		let reflog = match committer {
			Some(committer) => ReflogIntent::Log {
				committer,
				message: &message,
			},
			None => ReflogIntent::Skip,
		};
		// Validate HEAD and its reflog under the retained HEAD.lock before publishing the mount or
		// changing the index/worktree. A deterministic publication failure must leave an existing
		// module at the old commit rather than reporting an error after checkout has already landed.
		let prepared_head = head_lock.prepare_detached(entry.recorded, reflog).await?;
		if let Some(published_intent) = self
			.publish_module_mount(
				entry,
				&mount,
				&module_directory,
				cloned || entry.recovering,
				configuration,
				resolved_attachment_source.as_deref(),
			)
			.await?
		{
			intent_identity = Some(published_intent);
		}
		if mount.newly_attached {
			self.ensure_new_attachment_ready(entry, &mount)?;
		}
		let work_directory = mount
			.directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			})?;
		let work = CapWorkDir::from_dir(work_directory);
		let worktree = WorkTree::new_located(
			repository,
			work,
			self.layout.git_dir.join(&module_relative),
			self.worktree_root().join(&entry.declaration.path),
		);
		if completion_required {
			worktree.populate(target_tree).await?;
		} else if let Some(current) = current {
			let current_tree = worktree.repository().commit_tree(current).await?;
			worktree
				.checkout_merge(current_tree, target_tree, excludes_file.as_deref())
				.await?;
		} else {
			unreachable!("ordinary existing modules with unborn HEAD are rejected before publication");
		}
		self.ensure_mount_identity(entry, &mount)?;
		self.ensure_mount_marker(entry, &mount)?;
		self.ensure_module_identity(entry, &module_directory)?;
		prepared_head.finish().await?;
		self.ensure_mount_identity(entry, &mount)?;
		self.ensure_mount_marker(entry, &mount)?;
		self.ensure_module_identity(entry, &module_directory)?;
		if completion_required {
			worktree
				.repository()
				.objects()
				.file_store()
				.durability_barrier(&[
					DurabilityTarget::file("index"),
					DurabilityTarget::file("HEAD"),
					DurabilityTarget::directory(""),
				])
				.await
				.map_err(gitana_object_store::ObjectStoreError::from)?;
			self.ensure_mount_identity(entry, &mount)?;
			self.ensure_module_identity(entry, &module_directory)?;
			let expected = intent_identity.ok_or_else(|| {
				SubmoduleError::RecoveryRequired("completion requires a pinned staging intent".to_owned())
			})?;
			self.clear_control_dir(Some(expected))?;
		}
		Ok(if cloned {
			UpdateOutcomeState::Cloned
		} else {
			UpdateOutcomeState::CheckedOut
		})
	}

	async fn prepare_module<H: HashAlgorithm, T: RepositoryTransfer>(
		&self,
		entry: &Planned<H>,
		source: &str,
		transfer: &T,
	) -> Result<EntryIdentity, SubmoduleError> {
		let request = PrepareSource {
			source_url: source.to_owned(),
			persist_url: gitana_remote::redact_password(source),
			hash_kind: crate::object_id::kind::<H>(),
		};
		let resolved_source = transfer
			.resolve_source_identity(&request)
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		let prepared = transfer
			.prepare_source(request)
			.await
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		if prepared.resolved_source != resolved_source {
			return Err(SubmoduleError::Transfer(
				"submodule transfer source changed during preparation".to_owned(),
			));
		}
		let intent = stage_intent(
			entry,
			&prepared.resolved_source,
			IntentSourceContext::Superproject,
		);
		let intent_identity = self.prepare_control_dir(&intent)?;
		self
			.git
			.create_dir(STAGED_REPOSITORY)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(STAGED_REPOSITORY),
				source,
			})?;
		let stage_directory = self
			.open_git_subdir_nofollow(Path::new(STAGED_REPOSITORY))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(STAGED_REPOSITORY),
				source,
			})?;
		let transfer_directory = stage_directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(STAGED_REPOSITORY),
				source,
			})?;
		transfer
			.populate_prepared(
				prepared.source,
				PrepareRepository {
					git_dir: transfer_directory,
					display_git_dir: self.layout.git_dir.join(STAGED_REPOSITORY),
					hash_kind: crate::object_id::kind::<H>(),
					recorded: SubmoduleObjectId::from_typed(entry.recorded),
				},
			)
			.await
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		self
			.verify_staged::<H>(entry.recorded, &entry.declaration.name, &stage_directory)
			.await?;
		let target = Path::new("modules").join(&entry.declaration.name);
		let target_parent = self.ensure_git_parent_directories(&target)?;
		let target_name = target
			.file_name()
			.expect("validated module target has a final component");
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		match rename_directory_noreplace(&control, "repository", &target_parent, target_name) {
			Ok(()) => {}
			Err(source)
				if matches!(
					source.kind(),
					std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
				) =>
			{
				return Err(SubmoduleError::InvalidRepository(
					entry.declaration.name.clone(),
				));
			}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(&target),
					source,
				});
			}
		}
		let published = target_parent
			.open_dir_nofollow(target_name)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(&target),
				source,
			})?;
		if !same_directory_identity(&stage_directory, &published).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(&target),
				source,
			}
		})? {
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		sync_repository_publication_parents(&target, |parent| self.sync_git_directory(parent))?;
		Ok(intent_identity)
	}

	fn prepare_control_dir(&self, intent: &StageIntent) -> Result<EntryIdentity, SubmoduleError> {
		let mut created = false;
		match self.git.symlink_metadata(CONTROL_DIR) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"control path is not a directory".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				self
					.git
					.create_dir(CONTROL_DIR)
					.map_err(|source| SubmoduleError::Io {
						path: self.layout.git_dir.join(CONTROL_DIR),
						source,
					})?;
				created = true;
			}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				});
			}
		}
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		let previous = read_stage_intent(&control, &self.layout.git_dir.join(INTENT_FILE))?;
		let previous_identity = match previous {
			Some((previous, identity)) => {
				if !intent_matches_reprepare(&previous, intent) {
					return Err(SubmoduleError::RecoveryRequired(format!(
						"staging belongs to '{}' at '{}'",
						previous.name, previous.path
					)));
				}
				Some(identity)
			}
			None => {
				if safe_directory_exists(
					&control,
					Path::new(STAGED_REPOSITORY_NAME),
					&self.layout.git_dir.join(STAGED_REPOSITORY),
				)? {
					return Err(SubmoduleError::RecoveryRequired(
						"staged repository exists without an intent".to_owned(),
					));
				}
				None
			}
		};
		if safe_directory_exists(
			&control,
			Path::new(STAGED_REPOSITORY_NAME),
			&self.layout.git_dir.join(STAGED_REPOSITORY),
		)? {
			let staged = control
				.open_dir_nofollow(STAGED_REPOSITORY_NAME)
				.map_err(|source| SubmoduleError::Io {
					path: self.layout.git_dir.join(STAGED_REPOSITORY),
					source,
				})?;
			let identity = directory_identity(&staged).map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(STAGED_REPOSITORY),
				source,
			})?;
			remove_staged_repository(
				&control,
				identity,
				&self.layout.git_dir.join(STAGED_REPOSITORY),
			)?;
		}
		let bytes = serde_json::to_vec(intent)
			.map_err(|error| SubmoduleError::RecoveryRequired(error.to_string()))?;
		match control.symlink_metadata(INTENT_LOCK_NAME) {
			Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
				let identity = gitana_fs_native::entry_identity(&control, OsStr::new(INTENT_LOCK_NAME))
					.map_err(|source| SubmoduleError::Io {
						path: self.layout.git_dir.join(INTENT_LOCK),
						source,
					})?;
				remove_file_if_identity(&control, OsStr::new(INTENT_LOCK_NAME), identity).map_err(
					|source| {
						if source.kind() == std::io::ErrorKind::AlreadyExists {
							SubmoduleError::RecoveryRequired(
								"staging intent lock changed before cleanup".to_owned(),
							)
						} else {
							SubmoduleError::Io {
								path: self.layout.git_dir.join(INTENT_LOCK),
								source,
							}
						}
					},
				)?;
			}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"staging intent lock is not a regular file".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(INTENT_LOCK),
					source,
				});
			}
		}
		let mut options = OpenOptions::new();
		options.write(true).create_new(true);
		#[cfg(windows)]
		{
			use cap_std::fs::OpenOptionsExt as _;
			use windows_sys::Win32::Storage::FileSystem::{
				FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
			};
			options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
		}
		let mut file = control
			.open_with(INTENT_LOCK_NAME, &options)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(INTENT_LOCK),
				source,
			})?;
		let lock_identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(INTENT_LOCK),
			source,
		})?;
		let mut cleanup = ConditionalFileCleanup {
			directory: control.try_clone().map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?,
			name: INTENT_LOCK_NAME,
			identity: lock_identity,
			armed: true,
		};
		file
			.write_all(&bytes)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(INTENT_LOCK),
				source,
			})?;
		file.sync_all().map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(INTENT_LOCK),
			source,
		})?;
		let publication = publish_stage_intent(&control, lock_identity, previous_identity);
		publication.map_err(|source| {
			if source.kind() == std::io::ErrorKind::AlreadyExists {
				SubmoduleError::RecoveryRequired(
					"staging intent changed before conditional publication".to_owned(),
				)
			} else {
				SubmoduleError::Io {
					path: self.layout.git_dir.join(INTENT_FILE),
					source,
				}
			}
		})?;
		drop(file);
		cleanup.armed = false;
		self.sync_git_directory(Path::new(CONTROL_DIR))?;
		if created {
			self.sync_git_directory(Path::new(""))?;
		}
		Ok(lock_identity)
	}

	async fn verify_staged<H: HashAlgorithm>(
		&self,
		recorded: ObjectId<H>,
		name: &str,
		directory: &Dir,
	) -> Result<(), SubmoduleError> {
		self.ensure_staged_identity(name, directory)?;
		let files =
			LocalFileStore::from_dir(directory.try_clone().map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(STAGED_REPOSITORY),
				source,
			})?);
		if detect_hash_kind(&files).await? != crate::object_id::kind::<H>() {
			return Err(SubmoduleError::InvalidRepository(name.to_owned()));
		}
		let repository = Repository::<_, H>::new(ObjectStore::new(files));
		repository.commit_tree(recorded).await?;
		// The staged repository is not externally reachable until its directory is renamed into
		// `modules/`. Make the recorded closure and the metadata needed to reopen it durable before
		// that namespace publication; a failed barrier leaves the durable intent and private stage in
		// place for the normal recovery path.
		repository
			.durability_barrier_object_graph(recorded, &[])
			.await?;
		repository.durability_barrier_initialized().await?;
		self.ensure_staged_identity(name, directory)?;
		Ok(())
	}

	fn ensure_staged_identity(&self, name: &str, directory: &Dir) -> Result<(), SubmoduleError> {
		let current = self
			.open_git_subdir_nofollow(Path::new(STAGED_REPOSITORY))
			.map_err(|_| SubmoduleError::InvalidRepository(name.to_owned()))?;
		if !same_directory_identity(directory, &current).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(STAGED_REPOSITORY),
			source,
		})? {
			return Err(SubmoduleError::InvalidRepository(name.to_owned()));
		}
		Ok(())
	}

	async fn recover_stage<H: HashAlgorithm, C: ConfigurationProvider, T: RepositoryTransfer>(
		&self,
		plan: &mut [Planned<H>],
		configuration: &C,
		transfer: &T,
	) -> Result<Option<usize>, SubmoduleError> {
		match self.git.symlink_metadata(CONTROL_DIR) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"control path is not a directory".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				});
			}
		}
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		let (intent, intent_identity) =
			match read_stage_intent(&control, &self.layout.git_dir.join(INTENT_FILE))? {
				Some(intent) => intent,
				None => {
					if safe_directory_exists(
						&control,
						Path::new(STAGED_REPOSITORY_NAME),
						&self.layout.git_dir.join(STAGED_REPOSITORY),
					)? {
						return Err(SubmoduleError::RecoveryRequired(
							"staged repository exists without an intent".to_owned(),
						));
					}
					self.clear_control_dir(None)?;
					return Ok(None);
				}
			};
		let source_context = intent_source_context(&intent).ok_or_else(|| {
			let message = if matches!(intent.version, 1..=4) {
				format!(
					"staging intent version {} has an invalid source context",
					intent.version
				)
			} else {
				format!("unsupported staging intent version {}", intent.version)
			};
			SubmoduleError::RecoveryRequired(message)
		})?;
		let Some(recovery_index) = plan.iter().position(|entry| {
			entry.declaration.name == intent.name
				&& entry.declaration.path == intent.path
				&& entry.recorded.to_hex() == intent.recorded
		}) else {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"unfinished staging for '{}'",
				intent.name
			)));
		};
		let entry = &mut plan[recovery_index];
		let target = Path::new("modules").join(&entry.declaration.name);
		let target_exists =
			safe_directory_exists(&self.git, &target, &self.layout.git_dir.join(&target))?;
		let staged_exists = safe_directory_exists(
			&self.git,
			Path::new(STAGED_REPOSITORY),
			&self.layout.git_dir.join(STAGED_REPOSITORY),
		)?;
		// An intent with no repository name owns no durable content and is safe to abandon. Once either
		// name exists, its resolved endpoint identity remains binding and recovery must not silently
		// reuse the repository after an `insteadOf` rule selects a different source.
		if !target_exists && !staged_exists {
			self.clear_control_dir(Some(intent_identity))?;
			return Ok(None);
		}
		let (source, resolved) = match source_context {
			IntentSourceContext::Superproject => {
				let source = entry.source_url.as_deref().ok_or_else(|| {
					SubmoduleError::RecoveryRequired(format!(
						"unfinished staging source does not match '{}'",
						intent.name
					))
				})?;
				let request = PrepareSource {
					source_url: source.to_owned(),
					persist_url: gitana_remote::redact_password(source),
					hash_kind: crate::object_id::kind::<H>(),
				};
				let resolved = transfer
					.resolve_source_identity(&request)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				(source.to_owned(), resolved)
			}
			IntentSourceContext::Module => {
				if !target_exists || staged_exists {
					return Err(SubmoduleError::RecoveryRequired(format!(
						"module-scoped staging requires one published repository for '{}'",
						intent.name
					)));
				}
				let module_git_dir = self.layout.git_dir.join(&target);
				let module_directory =
					self
						.open_git_subdir_nofollow(&target)
						.map_err(|source| SubmoduleError::Io {
							path: module_git_dir.clone(),
							source,
						})?;
				let config_directory =
					module_directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module_git_dir.clone(),
							source,
						})?;
				let config = configuration
					.load_module_config(config_directory, &module_git_dir)
					.await?;
				self.ensure_module_identity(entry, &module_directory)?;
				let source = module_origin_url(&config, &entry.declaration.name)?;
				let fetch_source = FetchSource {
					source_url: source.clone(),
					worktree_dir: self.worktree_root().join(&entry.declaration.path),
					config,
				};
				let resolved = transfer
					.resolve_fetch_source_identity(&fetch_source)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				(source, resolved)
			}
		};
		if !intent_matches_source(&intent, source_context, &source, &resolved) {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"unfinished staging source does not match '{}'",
				intent.name
			)));
		}
		if entry.state.is_some() {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"unfinished staging for skipped submodule '{}'",
				intent.name
			)));
		}
		entry.intent_identity = Some(intent_identity);
		if target_exists && !staged_exists {
			entry.recovering = true;
			return Ok(Some(recovery_index));
		}
		if target_exists && staged_exists {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"both staged and published repositories exist for '{}'",
				intent.name
			)));
		}
		// An unpublished or not-yet-prepared matching intent must also run first. Its normal
		// preparation path safely discards any matching staged repository and retries it.
		Ok(Some(recovery_index))
	}

	fn clear_control_dir(
		&self,
		intent_identity: Option<EntryIdentity>,
	) -> Result<(), SubmoduleError> {
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		match intent_identity {
			Some(identity) => {
				remove_file_if_identity(&control, OsStr::new(INTENT_NAME), identity).map_err(|source| {
					if source.kind() == std::io::ErrorKind::AlreadyExists {
						SubmoduleError::RecoveryRequired(
							"staging intent changed before completion cleanup".to_owned(),
						)
					} else {
						SubmoduleError::Io {
							path: self.layout.git_dir.join(INTENT_FILE),
							source,
						}
					}
				})?;
			}
			None => match control.symlink_metadata(INTENT_NAME) {
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
				Ok(_) => {
					return Err(SubmoduleError::RecoveryRequired(
						"staging intent appeared before control cleanup".to_owned(),
					));
				}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: self.layout.git_dir.join(INTENT_FILE),
						source,
					});
				}
			},
		}
		match control.symlink_metadata(INTENT_LOCK_NAME) {
			Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
				let identity = gitana_fs_native::entry_identity(&control, OsStr::new(INTENT_LOCK_NAME))
					.map_err(|source| SubmoduleError::Io {
						path: self.layout.git_dir.join(INTENT_LOCK),
						source,
					})?;
				remove_file_if_identity(&control, OsStr::new(INTENT_LOCK_NAME), identity).map_err(
					|source| SubmoduleError::Io {
						path: self.layout.git_dir.join(INTENT_LOCK),
						source,
					},
				)?;
			}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"staging intent lock is not a regular file".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.layout.git_dir.join(INTENT_LOCK),
					source,
				});
			}
		}
		self.sync_git_directory(Path::new(CONTROL_DIR))?;
		let control_identity = directory_identity(&control).map_err(|source| SubmoduleError::Io {
			path: self.layout.git_dir.join(CONTROL_DIR),
			source,
		})?;
		retire_control_directory(&self.git, control_identity).map_err(|source| {
			if source.kind() == std::io::ErrorKind::AlreadyExists {
				SubmoduleError::RecoveryRequired(
					"staging control directory changed before cleanup".to_owned(),
				)
			} else {
				SubmoduleError::Io {
					path: self.layout.git_dir.join(CONTROL_DIR),
					source,
				}
			}
		})?;
		self.sync_git_directory(Path::new(""))?;
		Ok(())
	}

	fn sync_git_directory(&self, relative: &Path) -> Result<(), SubmoduleError> {
		let mut options = OpenOptions::new();
		#[cfg(not(windows))]
		options.read(true);
		#[cfg(windows)]
		{
			use cap_std::fs::OpenOptionsExt;
			use windows_sys::Win32::Storage::FileSystem::{
				FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
			};
			options
				.read(true)
				.write(true)
				.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
		}
		self
			.git
			.open_with(
				if relative.as_os_str().is_empty() {
					Path::new(".")
				} else {
					relative
				},
				&options,
			)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(relative),
				source,
			})?
			.sync_all()
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(relative),
				source,
			})
	}

	fn ensure_git_parent_directories(&self, target: &Path) -> Result<Dir, SubmoduleError> {
		let parent = target.parent().unwrap_or(Path::new(""));
		ensure_directory_components(
			&self.git,
			parent,
			&self.layout.git_dir,
			DirectoryNamespace::Module(target),
			sync_capability_directory,
		)
	}

	fn open_module_repository<H: HashAlgorithm>(
		&self,
		declaration: &SubmoduleDeclaration,
	) -> Result<(Repository<LocalFileStore, H>, Dir), SubmoduleError> {
		let relative = Path::new("modules").join(&declaration.name);
		let directory = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(declaration.name.clone()))?;
		let files =
			LocalFileStore::from_dir(directory.try_clone().map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(&relative),
				source,
			})?);
		let repository = Repository::new(ObjectStore::new(files));
		Ok((repository, directory))
	}

	fn module_pointers(
		&self,
		declaration: &SubmoduleDeclaration,
	) -> Result<ModulePointers, SubmoduleError> {
		let module = self.layout.git_dir.join("modules").join(&declaration.name);
		let mount = self.worktree_root().join(&declaration.path);
		let core_worktree = relative_path(&module, &mount).unwrap_or_else(|| mount.clone());
		let core_worktree = slash_path(&core_worktree)?;
		let target = relative_path(&mount, &module).unwrap_or_else(|| module.clone());
		let marker = format!("gitdir: {}\n", slash_path(&target)?);
		Ok(ModulePointers {
			core_worktree,
			marker,
		})
	}

	fn preflight_module_namespaces(
		&self,
		declaration: &SubmoduleDeclaration,
		pointers: &ModulePointers,
	) -> Result<(), SubmoduleError> {
		let module_relative = Path::new("modules").join(&declaration.name);
		validate_existing_directory_components(
			&self.git,
			&module_relative,
			&self.layout.git_dir,
			DirectoryNamespace::Module(&module_relative),
		)?;

		let Some(work_directory) = self.existing_mount_directory_nofollow(&declaration.path)? else {
			return Ok(());
		};
		let mount = self.worktree_root().join(&declaration.path);
		let work = CapWorkDir::from_dir(work_directory);
		match work.lstat(".git").map_err(|source| SubmoduleError::Io {
			path: mount.join(".git"),
			source,
		})? {
			None => {
				if !work
					.read_dir("")
					.map_err(|source| SubmoduleError::Io {
						path: mount,
						source,
					})?
					.is_empty()
				{
					return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
				}
			}
			Some(metadata) if metadata.kind.is_file() => {
				let current = work.read(".git").map_err(|source| SubmoduleError::Io {
					path: mount.join(".git"),
					source,
				})?;
				if current != pointers.marker.as_bytes()
					&& !std::str::from_utf8(&current)
						.ok()
						.and_then(parse_marker_target)
						.is_some_and(|target| self.marker_targets_expected(declaration, target))
				{
					return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
				}
			}
			Some(_) => return Err(SubmoduleError::ForeignMount(declaration.path.clone())),
		}
		Ok(())
	}

	async fn inspect_module_mount<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
	) -> Result<MountPlan, SubmoduleError> {
		let declaration = &entry.declaration;
		let mount = self.worktree_root().join(&declaration.path);
		let work_directory = self
			.existing_mount_directory_nofollow(&declaration.path)?
			.ok_or_else(|| SubmoduleError::ForeignMount(declaration.path.clone()))?;
		let work =
			CapWorkDir::from_dir(
				work_directory
					.try_clone()
					.map_err(|source| SubmoduleError::Io {
						path: mount.clone(),
						source,
					})?,
			);
		let marker = ".git";
		let (mounted, newly_attached, marker_snapshot) =
			match work.lstat(marker).map_err(|source| SubmoduleError::Io {
				path: mount.join(".git"),
				source,
			})? {
				None => {
					if !work
						.read_dir("")
						.map_err(|source| SubmoduleError::Io {
							path: mount.clone(),
							source,
						})?
						.is_empty()
					{
						return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
					}
					(false, true, MarkerSnapshot::Absent)
				}
				Some(metadata) if metadata.kind.is_file() => {
					let identity = marker_identity(&work_directory, &mount.join(".git"), &declaration.path)?;
					let marker_store =
						LocalFileStore::from_dir(work_directory.try_clone().map_err(|source| {
							SubmoduleError::Io {
								path: mount.clone(),
								source,
							}
						})?);
					let (current, _) = marker_store
						.read_path_versioned(marker)
						.await
						.map_err(gitana_object_store::ObjectStoreError::from)?;
					if marker_identity(&work_directory, &mount.join(".git"), &declaration.path)? != identity {
						return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
					}
					if current != entry.pointers.marker.as_bytes() {
						let equivalent = std::str::from_utf8(&current)
							.ok()
							.and_then(parse_marker_target)
							.is_some_and(|target| self.marker_targets_expected(declaration, target));
						if !equivalent {
							return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
						}
					}
					(
						true,
						false,
						MarkerSnapshot::File {
							identity,
							bytes: current,
						},
					)
				}
				Some(_) => return Err(SubmoduleError::ForeignMount(declaration.path.clone())),
			};
		Ok(MountPlan {
			directory: work_directory,
			mounted,
			newly_attached,
			marker: marker_snapshot,
		})
	}

	async fn revalidate_module_mount<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		previous: &MountPlan,
	) -> Result<MountPlan, SubmoduleError> {
		let current = self.inspect_module_mount(entry).await?;
		let same_identity =
			same_directory_identity(&previous.directory, &current.directory).map_err(|source| {
				SubmoduleError::Io {
					path: self.worktree_root().join(&entry.declaration.path),
					source,
				}
			})?;
		let same_state = previous.mounted == current.mounted
			&& previous.newly_attached == current.newly_attached
			&& previous.marker == current.marker;
		if !same_identity || !same_state {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		Ok(current)
	}

	fn ensure_mount_identity<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
	) -> Result<(), SubmoduleError> {
		let current = self
			.existing_mount_directory_nofollow(&entry.declaration.path)?
			.ok_or_else(|| SubmoduleError::ForeignMount(entry.declaration.path.clone()))?;
		if !same_directory_identity(&mount_plan.directory, &current).map_err(|source| {
			SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			}
		})? {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		Ok(())
	}

	fn ensure_new_attachment_ready<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
	) -> Result<(), SubmoduleError> {
		self.ensure_mount_identity(entry, mount_plan)?;
		let mut entries = mount_plan
			.directory
			.entries()
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			})?;
		let Some(first) = entries
			.next()
			.transpose()
			.map_err(|source| SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			})?
		else {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		};
		if first.file_name() != ".git"
			|| entries
				.next()
				.transpose()
				.map_err(|source| SubmoduleError::Io {
					path: self.worktree_root().join(&entry.declaration.path),
					source,
				})?
				.is_some()
		{
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		let work = CapWorkDir::from_dir(mount_plan.directory.try_clone().map_err(|source| {
			SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			}
		})?);
		let marker = work.read(".git").map_err(|source| SubmoduleError::Io {
			path: self
				.worktree_root()
				.join(&entry.declaration.path)
				.join(".git"),
			source,
		})?;
		let expected = match &mount_plan.marker {
			MarkerSnapshot::Absent => entry.pointers.marker.as_bytes(),
			MarkerSnapshot::File { identity, bytes } => {
				if marker_identity(
					&mount_plan.directory,
					&self
						.worktree_root()
						.join(&entry.declaration.path)
						.join(".git"),
					&entry.declaration.path,
				)? != *identity
				{
					return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
				}
				bytes.as_slice()
			}
		};
		if marker != expected {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		Ok(())
	}

	fn ensure_mount_marker<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
	) -> Result<(), SubmoduleError> {
		self.ensure_mount_identity(entry, mount_plan)?;
		let work = CapWorkDir::from_dir(mount_plan.directory.try_clone().map_err(|source| {
			SubmoduleError::Io {
				path: self.worktree_root().join(&entry.declaration.path),
				source,
			}
		})?);
		let marker = work.read(".git").map_err(|source| SubmoduleError::Io {
			path: self
				.worktree_root()
				.join(&entry.declaration.path)
				.join(".git"),
			source,
		})?;
		let expected = match &mount_plan.marker {
			MarkerSnapshot::Absent => entry.pointers.marker.as_bytes(),
			MarkerSnapshot::File { identity, bytes } => {
				if marker_identity(
					&mount_plan.directory,
					&self
						.worktree_root()
						.join(&entry.declaration.path)
						.join(".git"),
					&entry.declaration.path,
				)? != *identity
				{
					return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
				}
				bytes.as_slice()
			}
		};
		if marker != expected {
			return Err(SubmoduleError::ForeignMount(entry.declaration.path.clone()));
		}
		Ok(())
	}

	async fn publish_module_mount<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
		module_directory: &Dir,
		intent_durable: bool,
		configuration: &C,
		resolved_source: Option<&str>,
	) -> Result<Option<EntryIdentity>, SubmoduleError> {
		let declaration = &entry.declaration;
		let published_intent = if mount_plan.newly_attached && !intent_durable {
			let resolved = resolved_source.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"new mount publication requires a resolved source identity".to_owned(),
				)
			})?;
			Some(self.prepare_control_dir(&stage_intent(entry, resolved, IntentSourceContext::Module))?)
		} else {
			None
		};

		// Revalidate the complete marker/emptiness snapshot immediately before the only repository
		// mutation in this publication step. A later marker race is still possible, so the config
		// edit below is paired with a conditional rollback token.
		self.revalidate_module_mount(entry, mount_plan).await?;
		self.ensure_mount_identity(entry, mount_plan)?;
		self.ensure_module_identity(entry, module_directory)?;
		let module_path = self.layout.git_dir.join("modules").join(&declaration.name);
		let config_path = module_path.join("config");
		let edit_directory = module_directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: module_path.clone(),
				source,
			})?;
		let rollback_directory = module_directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: module_path.clone(),
				source,
			})?;
		let edit = configuration
			.set_module_worktree(edit_directory, &config_path, &entry.pointers.core_worktree)
			.await?;
		let publication = async {
			self.ensure_module_identity(entry, module_directory)?;
			self.publish_mount_marker(entry, mount_plan).await
		}
		.await;
		if let Err(error) = publication {
			if let Err(rollback) = configuration
				.rollback_module_worktree(rollback_directory, &config_path, edit)
				.await
			{
				return Err(SubmoduleError::RecoveryRequired(format!(
					"mount publication failed ({error}); restoring core.worktree failed ({rollback})"
				)));
			}
			return Err(error);
		}
		Ok(published_intent)
	}

	async fn publish_mount_marker<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
	) -> Result<(), SubmoduleError> {
		let declaration = &entry.declaration;
		self.revalidate_module_mount(entry, mount_plan).await?;
		match &mount_plan.marker {
			MarkerSnapshot::Absent => publish_new_mount_marker(
				&mount_plan.directory,
				entry.pointers.marker.as_bytes(),
				&self.worktree_root().join(&declaration.path).join(".git"),
				&declaration.path,
			)?,
			// A canonical or path-equivalent existing marker is already a valid mount. Preserve its
			// exact bytes and identity instead of replacing an entry that another process may race.
			MarkerSnapshot::File { .. } => return Ok(()),
		}
		let marker_store =
			LocalFileStore::from_dir(mount_plan.directory.try_clone().map_err(|source| {
				SubmoduleError::Io {
					path: self.worktree_root().join(&declaration.path),
					source,
				}
			})?);
		marker_store
			.durability_barrier(&[
				DurabilityTarget::file(".git"),
				DurabilityTarget::directory(""),
			])
			.await
			.map_err(gitana_object_store::ObjectStoreError::from)?;
		Ok(())
	}

	fn ensure_module_identity<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
		module_directory: &Dir,
	) -> Result<(), SubmoduleError> {
		let relative = Path::new("modules").join(&entry.declaration.name);
		let current = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(entry.declaration.name.clone()))?;
		if !same_directory_identity(module_directory, &current).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(&relative),
				source,
			}
		})? {
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		Ok(())
	}

	fn ensure_empty_mount(&self, path: &str) -> Result<(), SubmoduleError> {
		self.ensure_mount_directory(path, true)
	}

	fn ensure_mount_directory(&self, path: &str, require_empty: bool) -> Result<(), SubmoduleError> {
		let current = ensure_directory_components(
			&self.work,
			Path::new(path),
			self.worktree_root(),
			DirectoryNamespace::Mount(path),
			sync_capability_directory,
		)?;
		if require_empty
			&& current
				.entries()
				.map_err(|source| SubmoduleError::Io {
					path: self.worktree_root().join(path),
					source,
				})?
				.next()
				.is_some()
		{
			return Err(SubmoduleError::ForeignMount(path.to_owned()));
		}
		Ok(())
	}

	fn acquire_update_lock(&self) -> Result<UpdateLockGuard, SubmoduleError> {
		let path = self.layout.git_dir.join(UPDATE_LOCK);
		#[cfg(unix)]
		let directory_lock = {
			let mut options = OpenOptions::new();
			options.read(true);
			let directory = self
				.git
				.open_with(".", &options)
				.map_err(|source| SubmoduleError::Io {
					path: self.layout.git_dir.clone(),
					source,
				})?
				.into_std();
			File::try_lock(&directory).map_err(|error| match error {
				std::fs::TryLockError::WouldBlock => SubmoduleError::UpdateLocked,
				std::fs::TryLockError::Error(source) => SubmoduleError::Io {
					path: self.layout.git_dir.clone(),
					source,
				},
			})?;
			Some(directory)
		};
		#[cfg(not(unix))]
		let directory_lock = None;
		match self.git.symlink_metadata(UPDATE_LOCK) {
			Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
			Ok(_) => {
				return Err(SubmoduleError::RecoveryRequired(
					"submodule update lock is not a regular file".to_owned(),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(source) => {
				return Err(SubmoduleError::Io { path, source });
			}
		}
		let mut options = OpenOptions::new();
		options.read(true).write(true).create(true);
		options.follow(FollowSymlinks::No);
		#[cfg(windows)]
		{
			use cap_std::fs::OpenOptionsExt as _;
			use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
			// Omitting FILE_SHARE_DELETE keeps the selected entry attached to its name while
			// this guard is alive.
			options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
		}
		let file = self
			.git
			.open_with(UPDATE_LOCK, &options)
			.map_err(|source| SubmoduleError::Io {
				path: path.clone(),
				source,
			})?;
		let metadata = file.metadata().map_err(|source| SubmoduleError::Io {
			path: path.clone(),
			source,
		})?;
		if !metadata.is_file() || metadata.file_type().is_symlink() {
			return Err(SubmoduleError::RecoveryRequired(
				"submodule update lock is not a regular file".to_owned(),
			));
		}
		let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: path.clone(),
			source,
		})?;
		let named_lock = file.into_std();
		File::try_lock(&named_lock).map_err(|error| match error {
			std::fs::TryLockError::WouldBlock => SubmoduleError::UpdateLocked,
			std::fs::TryLockError::Error(source) => SubmoduleError::Io {
				path: path.clone(),
				source,
			},
		})?;
		let guard = UpdateLockGuard {
			directory: self.git.try_clone().map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.clone(),
				source,
			})?,
			identity,
			display_path: path,
			_directory_lock: directory_lock,
			_named_lock: named_lock,
		};
		guard.validate()?;
		Ok(guard)
	}
}

/// Publish a complete marker only if the final name is still absent.
///
/// Writing the final name with `create_new` would expose partial contents, while checking absence
/// before a normal rename would let that rename overwrite a concurrent writer. A source-conditioned
/// no-replace rename makes the final name appear atomically and fails if any entry won the name first.
/// All operations stay relative to the retained mount directory.
fn publish_new_mount_marker(
	directory: &Dir,
	bytes: &[u8],
	display: &Path,
	submodule_path: &str,
) -> Result<(), SubmoduleError> {
	for _ in 0..MARKER_TEMP_ATTEMPTS {
		let sequence = MARKER_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
		let temporary = format!(".git.gitana-marker.{}.{}", std::process::id(), sequence);
		let mut options = OpenOptions::new();
		options.write(true).create_new(true);
		let mut file = match directory.open_with(&temporary, &options) {
			Ok(file) => file,
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: display.to_owned(),
					source,
				});
			}
		};
		let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?;
		let prepared = file.write_all(bytes).and_then(|()| file.sync_all());
		drop(file);
		if let Err(source) = prepared {
			let _ = remove_file_if_identity(directory, OsStr::new(&temporary), identity);
			return Err(SubmoduleError::Io {
				path: display.to_owned(),
				source,
			});
		}
		let published = rename_noreplace_if_identity(
			directory,
			OsStr::new(&temporary),
			identity,
			directory,
			OsStr::new(".git"),
		);
		return match published {
			Ok(()) => Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				let _ = remove_file_if_identity(directory, OsStr::new(&temporary), identity);
				Err(SubmoduleError::ForeignMount(submodule_path.to_owned()))
			}
			Err(source) => {
				let _ = remove_file_if_identity(directory, OsStr::new(&temporary), identity);
				Err(SubmoduleError::Io {
					path: display.to_owned(),
					source,
				})
			}
		};
	}
	Err(SubmoduleError::Io {
		path: display.to_owned(),
		source: std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"could not reserve a private mount-marker name",
		),
	})
}

/// Remove the active recovery name while retaining any identity-quarantined contents as one
/// private directory. A retry therefore sees no active intent without recursively deleting state
/// that a concurrent writer may have replaced.
fn retire_control_directory(directory: &Dir, expected: EntryIdentity) -> std::io::Result<()> {
	for _ in 0..MARKER_TEMP_ATTEMPTS {
		let sequence = CONTROL_RETIRE_COUNTER.fetch_add(1, Ordering::Relaxed);
		let retired = format!(
			".gitana-submodule-update-retired.{}.{}",
			std::process::id(),
			sequence
		);
		match directory.symlink_metadata(&retired) {
			Ok(_) => continue,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error),
		}
		match rename_noreplace_if_identity(
			directory,
			OsStr::new(CONTROL_DIR),
			expected,
			directory,
			OsStr::new(&retired),
		) {
			Ok(()) => return Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				if gitana_fs_native::entry_identity(directory, OsStr::new(CONTROL_DIR))
					.is_ok_and(|identity| identity == expected)
				{
					continue;
				}
				return Err(error);
			}
			Err(error) => return Err(error),
		}
	}
	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"could not reserve a private retired recovery-directory name",
	))
}

#[cfg(any(unix, windows))]
fn marker_identity(
	directory: &Dir,
	display: &Path,
	submodule_path: &str,
) -> Result<MarkerIdentity, SubmoduleError> {
	use cap_fs_ext::MetadataExt;

	let metadata = directory
		.symlink_metadata(".git")
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?;
	if !metadata.is_file() || metadata.file_type().is_symlink() {
		return Err(SubmoduleError::ForeignMount(submodule_path.to_owned()));
	}
	Ok(MarkerIdentity {
		device: metadata.dev(),
		inode: metadata.ino(),
	})
}

#[cfg(not(any(unix, windows)))]
fn marker_identity(
	_directory: &Dir,
	display: &Path,
	_submodule_path: &str,
) -> Result<MarkerIdentity, SubmoduleError> {
	Err(SubmoduleError::Io {
		path: display.to_owned(),
		source: std::io::Error::new(
			std::io::ErrorKind::Unsupported,
			"stable mount-marker identity is unavailable on this platform",
		),
	})
}

fn ensure_directory_components<F>(
	root: &Dir,
	relative: &Path,
	display_root: &Path,
	namespace: DirectoryNamespace<'_>,
	mut sync_parent: F,
) -> Result<Dir, SubmoduleError>
where
	F: FnMut(&Dir, &Path) -> Result<(), SubmoduleError>,
{
	let mut current = root.try_clone().map_err(|source| SubmoduleError::Io {
		path: display_root.to_owned(),
		source,
	})?;
	let mut traversed = PathBuf::new();
	for component in relative.components() {
		let Component::Normal(component) = component else {
			return Err(namespace.unsafe_component());
		};
		let parent_display = display_root.join(&traversed);
		traversed.push(component);
		match current.symlink_metadata(component) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
			Ok(_) => return Err(namespace.occupied()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				current
					.create_dir(component)
					.map_err(|source| SubmoduleError::Io {
						path: display_root.join(&traversed),
						source,
					})?;
				// The child entry is not durable until its containing directory is flushed. Do
				// that before descending so a completed deeper publication can never outlive an
				// unpersisted ancestor namespace entry.
				sync_parent(&current, &parent_display)?;
			}
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: display_root.join(&traversed),
					source,
				});
			}
		}
		current = current
			.open_dir_nofollow(component)
			.map_err(|source| SubmoduleError::Io {
				path: display_root.join(&traversed),
				source,
			})?;
	}
	Ok(current)
}

fn validate_existing_directory_components(
	root: &Dir,
	relative: &Path,
	display_root: &Path,
	namespace: DirectoryNamespace<'_>,
) -> Result<Option<Dir>, SubmoduleError> {
	let mut current = root.try_clone().map_err(|source| SubmoduleError::Io {
		path: display_root.to_owned(),
		source,
	})?;
	let mut traversed = PathBuf::new();
	for component in relative.components() {
		let Component::Normal(component) = component else {
			return Err(namespace.unsafe_component());
		};
		traversed.push(component);
		match current.symlink_metadata(component) {
			Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
			Ok(_) => return Err(namespace.occupied()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: display_root.join(&traversed),
					source,
				});
			}
		}
		current = current
			.open_dir_nofollow(component)
			.map_err(|source| SubmoduleError::Io {
				path: display_root.join(&traversed),
				source,
			})?;
	}
	Ok(Some(current))
}

fn sync_capability_directory(directory: &Dir, display: &Path) -> Result<(), SubmoduleError> {
	let mut options = OpenOptions::new();
	#[cfg(not(windows))]
	options.read(true);
	#[cfg(windows)]
	{
		use cap_std::fs::OpenOptionsExt;
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
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?
		.sync_all()
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})
}

fn sync_repository_publication_parents(
	target: &Path,
	mut sync: impl FnMut(&Path) -> Result<(), SubmoduleError>,
) -> Result<(), SubmoduleError> {
	// Persist the destination first so an interruption between barriers can at worst retain a
	// duplicate source name for recovery, rather than durably remove the only repository name.
	sync(target.parent().unwrap_or(Path::new("")))?;
	sync(Path::new(CONTROL_DIR))
}

/// Move a prepared directory into a retained destination parent without replacing any entry that
/// won the final name.
fn rename_directory_noreplace(
	source: &Dir,
	source_name: &str,
	destination: &Dir,
	destination_name: &std::ffi::OsStr,
) -> std::io::Result<()> {
	rename_noreplace(
		source,
		OsStr::new(source_name),
		destination,
		destination_name,
	)
}

impl UpdateFailure {
	fn preflight(source: SubmoduleError) -> Self {
		Self {
			completed: UpdateReport::default(),
			module: None,
			source,
		}
	}

	fn after_init(report: &UpdateReport, source: SubmoduleError) -> Self {
		Self {
			completed: report.clone(),
			module: None,
			source,
		}
	}
}

fn configured_update_strategy(
	config: &gitana_config::GitConfig,
	name: &str,
) -> Result<Option<String>, SubmoduleError> {
	match config.get_raw("submodule", Some(name), "update") {
		Some(Some(strategy)) => Ok(Some(strategy.to_owned())),
		Some(None) => Err(SubmoduleError::MissingValue(format!(
			"submodule.{name}.update"
		))),
		None => Ok(None),
	}
}

fn module_origin_url(
	config: &gitana_config::GitConfig,
	module: &str,
) -> Result<String, SubmoduleError> {
	crate::remote_url::first_fetch_url(config, "origin")?
		.map(str::to_owned)
		.ok_or_else(|| SubmoduleError::MissingModuleOrigin(module.to_owned()))
}

fn outcome<H: HashAlgorithm>(entry: &Planned<H>, state: UpdateOutcomeState) -> UpdateOutcome {
	UpdateOutcome {
		name: entry.declaration.name.clone(),
		path: entry.declaration.path.clone(),
		recorded: SubmoduleObjectId::from_typed(entry.recorded),
		state,
	}
}

fn source_fingerprint(source: &str) -> String {
	let safe = gitana_remote::redact_password(source);
	let digest = Sha256Digest::digest(safe.as_bytes());
	digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn legacy_source_fingerprint(source: &str) -> String {
	let safe = gitana_remote::anonymize_url(source);
	let digest = Sha256Digest::digest(safe.as_bytes());
	digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stage_intent<H: HashAlgorithm>(
	entry: &Planned<H>,
	source: &str,
	source_context: IntentSourceContext,
) -> StageIntent {
	StageIntent {
		version: 4,
		name: entry.declaration.name.clone(),
		path: entry.declaration.path.clone(),
		recorded: entry.recorded.to_hex(),
		source_fingerprint: source_fingerprint(source),
		source_context: Some(source_context),
	}
}

fn intent_matches_reprepare(previous: &StageIntent, current: &StageIntent) -> bool {
	intent_source_context(previous) == Some(IntentSourceContext::Superproject)
		&& current.version == 4
		&& current.source_context == Some(IntentSourceContext::Superproject)
		&& previous.name == current.name
		&& previous.path == current.path
		&& previous.recorded == current.recorded
		&& previous.source_fingerprint == current.source_fingerprint
}

fn intent_source_context(intent: &StageIntent) -> Option<IntentSourceContext> {
	match intent.version {
		1..=3 if intent.source_context.is_none() => Some(IntentSourceContext::Superproject),
		4 => intent.source_context,
		_ => None,
	}
}

fn intent_matches_source(
	intent: &StageIntent,
	source_context: IntentSourceContext,
	source: &str,
	resolved: &str,
) -> bool {
	let resolved = source_fingerprint(resolved);
	match intent.version {
		4 => intent.source_context == Some(source_context) && resolved == intent.source_fingerprint,
		3 if source_context == IntentSourceContext::Superproject && intent.source_context.is_none() => {
			resolved == intent.source_fingerprint
		}
		2 if source_context == IntentSourceContext::Superproject && intent.source_context.is_none() => {
			let raw = source_fingerprint(source);
			raw == resolved && raw == intent.source_fingerprint
		}
		1 if source_context == IntentSourceContext::Superproject && intent.source_context.is_none() => {
			let legacy = legacy_source_fingerprint(source);
			legacy == source_fingerprint(source)
				&& legacy == resolved
				&& legacy == intent.source_fingerprint
		}
		_ => false,
	}
}

fn safe_directory_exists(
	directory: &Dir,
	relative: &Path,
	display: &Path,
) -> Result<bool, SubmoduleError> {
	match directory.symlink_metadata(relative) {
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
		Ok(_) => Err(SubmoduleError::RecoveryRequired(format!(
			"{} is not a directory",
			display.display()
		))),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(source) => Err(SubmoduleError::Io {
			path: display.to_owned(),
			source,
		}),
	}
}

fn read_stage_intent(
	control: &Dir,
	display: &Path,
) -> Result<Option<(StageIntent, EntryIdentity)>, SubmoduleError> {
	match control.symlink_metadata(INTENT_NAME) {
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"staging intent is not a regular file".to_owned(),
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
		.open_with(INTENT_NAME, &options)
		.map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})?;
	let metadata = file.metadata().map_err(|source| SubmoduleError::Io {
		path: display.to_owned(),
		source,
	})?;
	if !metadata.is_file() || metadata.file_type().is_symlink() {
		return Err(SubmoduleError::RecoveryRequired(
			"staging intent is not a regular file".to_owned(),
		));
	}
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
		SubmoduleError::RecoveryRequired(format!("invalid staging intent: {error}"))
	})?;
	Ok(Some((intent, identity)))
}

fn publish_stage_intent(
	control: &Dir,
	lock_identity: EntryIdentity,
	previous_identity: Option<EntryIdentity>,
) -> std::io::Result<()> {
	match previous_identity {
		Some(expected) => replace_if_identities(
			control,
			OsStr::new(INTENT_LOCK_NAME),
			lock_identity,
			OsStr::new(INTENT_NAME),
			expected,
		),
		None => rename_noreplace_if_identity(
			control,
			OsStr::new(INTENT_LOCK_NAME),
			lock_identity,
			control,
			OsStr::new(INTENT_NAME),
		),
	}
}

fn remove_staged_repository(
	control: &Dir,
	expected: EntryIdentity,
	display: &Path,
) -> Result<(), SubmoduleError> {
	remove_dir_all_if_identity(control, OsStr::new(STAGED_REPOSITORY_NAME), expected).map_err(
		|source| {
			if source.kind() == std::io::ErrorKind::AlreadyExists {
				SubmoduleError::RecoveryRequired(
					"staged repository changed before recovery cleanup".to_owned(),
				)
			} else {
				SubmoduleError::Io {
					path: display.to_owned(),
					source,
				}
			}
		},
	)
}

fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
	let from: Vec<Component<'_>> = from.components().collect();
	let to: Vec<Component<'_>> = to.components().collect();
	let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
	if common == 0 {
		return None;
	}
	let mut relative = PathBuf::new();
	for _ in common..from.len() {
		relative.push("..");
	}
	for component in &to[common..] {
		relative.push(component.as_os_str());
	}
	Some(relative)
}

fn slash_path(path: &Path) -> Result<String, SubmoduleError> {
	let path = path
		.to_str()
		.ok_or_else(|| SubmoduleError::UnrepresentablePointerPath(path.to_owned()))?;
	#[cfg(windows)]
	let path = path.replace('\\', "/");
	#[cfg(not(windows))]
	let path = path.to_owned();
	Ok(path)
}

async fn operation_in_progress<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
) -> Result<Option<&'static str>, SubmoduleError> {
	if repository.merge_head().await?.is_some() {
		return Ok(Some("merge"));
	}
	if repository.cherry_pick_head().await?.is_some() {
		return Ok(Some("cherry-pick"));
	}
	if repository.revert_head().await?.is_some() {
		return Ok(Some("revert"));
	}
	if repository.rebase_in_progress().await? {
		return Ok(Some("rebase"));
	}
	let store = repository.objects().file_store();
	let rebase_merge = store
		.is_dir("rebase-merge")
		.await
		.map_err(gitana_repository::RepositoryError::from)?;
	let rebase_apply = store
		.is_dir("rebase-apply")
		.await
		.map_err(gitana_repository::RepositoryError::from)?;
	if rebase_merge || rebase_apply {
		return Ok(Some("rebase"));
	}
	Ok(None)
}

#[cfg(test)]
mod tests {
	#[cfg(not(windows))]
	use super::slash_path;
	use super::{
		DirectoryNamespace, IntentSourceContext, MarkerSnapshot, Planned, StageIntent, UPDATE_LOCK,
		ensure_directory_components, intent_matches_reprepare, intent_matches_source,
		intent_source_context, legacy_source_fingerprint, marker_identity, module_origin_url,
		publish_new_mount_marker, publish_stage_intent, remove_staged_repository,
		rename_directory_noreplace, source_fingerprint, sync_repository_publication_parents,
	};
	use crate::{
		ConfigViews, ConfigurationProvider, InitConfigResult, InitConfigUpdate, SubmoduleContext,
		SubmoduleDeclaration, SubmoduleError,
	};
	use cap_std::{ambient_authority, fs::Dir};
	use gitana_config::GitConfig;
	use gitana_object::{HashKind, ObjectId, Sha256};
	use gitana_repository_layout::RepositoryLayout;
	use std::path::{Path, PathBuf};

	fn intent(version: u32, source_fingerprint: String) -> StageIntent {
		StageIntent {
			version,
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			recorded: "00".to_owned(),
			source_fingerprint,
			source_context: (version == 4).then_some(IntentSourceContext::Superproject),
		}
	}

	#[test]
	fn recovery_identity_keeps_usernames_but_not_passwords() {
		let alice = source_fingerprint("ssh://alice:one@host/repository");
		let same_alice = source_fingerprint("ssh://alice:two@host/repository");
		let bob = source_fingerprint("ssh://bob:one@host/repository");
		assert_eq!(alice, same_alice);
		assert_ne!(alice, bob);
		assert!(intent_matches_source(
			&intent(3, alice),
			IntentSourceContext::Superproject,
			"ssh://alice:different@host/repository",
			"ssh://alice@host/repository",
		));
	}

	#[test]
	fn intent_publication_preserves_a_replacement_after_validation() {
		let temporary = tempfile::tempdir().unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		control.write("intent.json", b"matching").unwrap();
		let expected =
			gitana_fs_native::entry_identity(&control, std::ffi::OsStr::new("intent.json")).unwrap();
		control
			.rename("intent.json", &control, "old-intent")
			.unwrap();
		control.write("intent.json", b"foreign").unwrap();
		control.write("intent.lock", b"new").unwrap();
		let lock_identity =
			gitana_fs_native::entry_identity(&control, std::ffi::OsStr::new("intent.lock")).unwrap();

		assert!(publish_stage_intent(&control, lock_identity, Some(expected)).is_err());
		assert_eq!(control.read("intent.json").unwrap(), b"foreign");
		assert_eq!(control.read("intent.lock").unwrap(), b"new");
	}

	#[test]
	fn intent_publication_rejects_a_replaced_lock() {
		let temporary = tempfile::tempdir().unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		control.write("intent.lock", b"owned").unwrap();
		let expected =
			gitana_fs_native::entry_identity(&control, std::ffi::OsStr::new("intent.lock")).unwrap();
		control
			.rename("intent.lock", &control, "owned-lock")
			.unwrap();
		control.write("intent.lock", b"foreign").unwrap();

		assert!(publish_stage_intent(&control, expected, None).is_err());
		assert!(control.symlink_metadata("intent.json").is_err());
		assert_eq!(control.read("intent.lock").unwrap(), b"foreign");
	}

	#[test]
	fn staged_cleanup_preserves_a_replacement_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let control = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		control.create_dir("repository").unwrap();
		let staged = control.open_dir("repository").unwrap();
		let expected = gitana_fs_native::directory_identity(&staged).unwrap();
		control
			.rename("repository", &control, "old-repository")
			.unwrap();
		control.create_dir("repository").unwrap();
		control.write("repository/keep", b"foreign").unwrap();

		assert!(matches!(
			remove_staged_repository(&control, expected, Path::new("repository")),
			Err(SubmoduleError::RecoveryRequired(_))
		));
		assert_eq!(control.read("repository/keep").unwrap(), b"foreign");
	}

	#[test]
	fn legacy_recovery_is_accepted_only_when_userinfo_was_not_erased() {
		let plain = "https://host/repository";
		assert!(intent_matches_source(
			&intent(1, source_fingerprint(plain)),
			IntentSourceContext::Superproject,
			plain,
			plain,
		));

		let alice = "ssh://alice@host/repository";
		let legacy = legacy_source_fingerprint(alice);
		assert!(!intent_matches_source(
			&intent(1, legacy),
			IntentSourceContext::Superproject,
			alice,
			alice
		));
	}

	#[test]
	fn legacy_recovery_reprepare_matches_semantic_identity_and_upgrades_to_v4() {
		let fingerprint = source_fingerprint("https://host/repository");
		let legacy = intent(1, fingerprint.clone());
		let current = intent(4, fingerprint);
		assert!(intent_matches_reprepare(&legacy, &current));

		let mut wrong_path = legacy.clone();
		wrong_path.path = "modules/two".to_owned();
		assert!(!intent_matches_reprepare(&wrong_path, &current));
		let mut module = current.clone();
		module.source_context = Some(IntentSourceContext::Module);
		assert!(!intent_matches_reprepare(&module, &current));
		assert!(!intent_matches_reprepare(
			&intent(5, current.source_fingerprint.clone()),
			&current
		));
	}

	#[test]
	fn v4_recovery_requires_the_recorded_source_context() {
		let endpoint = "https://host/repository";
		let superproject = intent(4, source_fingerprint(endpoint));
		assert_eq!(
			intent_source_context(&superproject),
			Some(IntentSourceContext::Superproject)
		);
		assert!(intent_matches_source(
			&superproject,
			IntentSourceContext::Superproject,
			endpoint,
			endpoint,
		));
		assert!(!intent_matches_source(
			&superproject,
			IntentSourceContext::Module,
			endpoint,
			endpoint,
		));

		let mut missing = superproject;
		missing.source_context = None;
		assert_eq!(intent_source_context(&missing), None);
	}

	#[test]
	fn v2_recovery_requires_raw_and_resolved_sources_to_be_equivalent() {
		let direct = "https://host/repository";
		assert!(intent_matches_source(
			&intent(2, source_fingerprint(direct)),
			IntentSourceContext::Superproject,
			direct,
			direct,
		));

		let alias = "module-alias:";
		assert!(!intent_matches_source(
			&intent(2, source_fingerprint(alias)),
			IntentSourceContext::Superproject,
			alias,
			"https://host/repository",
		));
	}

	#[test]
	fn module_origin_uses_the_first_url_after_the_last_empty_reset() {
		let multiple =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = first\n\turl = second\n")
				.unwrap();
		assert_eq!(module_origin_url(&multiple, "one").unwrap(), "first");

		let reset = gitana_config::GitConfig::parse(
			"[remote \"origin\"]\n\turl = stale\n\turl =\n\turl = live\n\turl = ignored\n",
		)
		.unwrap();
		assert_eq!(module_origin_url(&reset, "one").unwrap(), "live");
	}

	#[test]
	fn module_origin_rejects_valueless_or_absent_surviving_urls() {
		let valueless =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = good\n\turl\n").unwrap();
		assert!(
			module_origin_url(&valueless, "one")
				.unwrap_err()
				.to_string()
				.contains("missing value")
		);

		let reset =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = stale\n\turl =\n").unwrap();
		assert!(
			module_origin_url(&reset, "one")
				.unwrap_err()
				.to_string()
				.contains("has no remote.origin.url")
		);
	}

	#[tokio::test]
	async fn mount_revalidation_rejects_content_marker_and_identity_changes() {
		let (temporary, context, entry) = mount_fixture("content");
		let before = context.inspect_module_mount(&entry).await.unwrap();
		std::fs::write(
			temporary.path().join("work/modules/one/file"),
			b"concurrent",
		)
		.unwrap();
		assert!(matches!(
			context.revalidate_module_mount(&entry, &before).await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("marker");
		let before = context.inspect_module_mount(&entry).await.unwrap();
		std::fs::write(
			temporary.path().join("work/modules/one/.git"),
			entry.pointers.marker.as_bytes(),
		)
		.unwrap();
		assert!(matches!(
			context.revalidate_module_mount(&entry, &before).await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("identity");
		let before = context.inspect_module_mount(&entry).await.unwrap();
		std::fs::rename(
			temporary.path().join("work/modules/one"),
			temporary.path().join("work/modules/old"),
		)
		.unwrap();
		std::fs::create_dir(temporary.path().join("work/modules/one")).unwrap();
		assert!(matches!(
			context.revalidate_module_mount(&entry, &before).await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("marker-identity");
		let marker = temporary.path().join("work/modules/one/.git");
		std::fs::write(&marker, entry.pointers.marker.as_bytes()).unwrap();
		let before = context.inspect_module_mount(&entry).await.unwrap();
		std::fs::remove_file(&marker).unwrap();
		std::fs::write(&marker, entry.pointers.marker.as_bytes()).unwrap();
		assert!(matches!(
			context.revalidate_module_mount(&entry, &before).await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
	}

	#[tokio::test]
	async fn conditional_marker_publish_preserves_a_concurrent_marker() {
		let (temporary, context, entry) = mount_fixture("conditional-marker");
		let before = context.inspect_module_mount(&entry).await.unwrap();
		let marker = temporary.path().join("work/modules/one/.git");
		std::fs::write(&marker, b"foreign marker\n").unwrap();

		assert!(matches!(
			context.publish_mount_marker(&entry, &before).await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
		assert_eq!(std::fs::read(marker).unwrap(), b"foreign marker\n");
	}

	struct MarkerRacingConfiguration {
		marker: PathBuf,
		marker_bytes: Vec<u8>,
	}

	impl ConfigurationProvider for MarkerRacingConfiguration {
		type ModuleWorktreeEdit = (Vec<u8>, Vec<u8>);

		async fn apply_init(
			&self,
			_updates: &[InitConfigUpdate],
		) -> Result<InitConfigResult, SubmoduleError> {
			unreachable!()
		}

		async fn reload(&self) -> Result<GitConfig, SubmoduleError> {
			unreachable!()
		}

		async fn load_module_config(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
		) -> Result<GitConfig, SubmoduleError> {
			unreachable!()
		}

		async fn module_hash_kind(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
		) -> Result<HashKind, SubmoduleError> {
			unreachable!()
		}

		async fn load_module_excludes(
			&self,
			_config: &GitConfig,
			_worktree_root: &Path,
		) -> Result<Option<String>, SubmoduleError> {
			unreachable!()
		}

		async fn set_module_worktree(
			&self,
			git_dir: Dir,
			_display_path: &Path,
			_worktree: &str,
		) -> Result<Self::ModuleWorktreeEdit, SubmoduleError> {
			let before = git_dir.read("config").unwrap();
			let after = b"[core]\n\tworktree = ../../modules/one\n".to_vec();
			git_dir.write("config", &after).unwrap();
			let _ = std::fs::remove_file(&self.marker);
			std::fs::write(&self.marker, &self.marker_bytes).unwrap();
			Ok((before, after))
		}

		async fn rollback_module_worktree(
			&self,
			git_dir: Dir,
			_display_path: &Path,
			edit: Self::ModuleWorktreeEdit,
		) -> Result<(), SubmoduleError> {
			if git_dir.read("config").unwrap() != edit.1 {
				return Err(SubmoduleError::Configuration(
					"module config changed before rollback".to_owned(),
				));
			}
			git_dir.write("config", &edit.0).unwrap();
			Ok(())
		}
	}

	#[tokio::test]
	async fn marker_race_rolls_back_core_worktree_without_replacing_the_marker() {
		let (temporary, context, entry) = mount_fixture("marker-config-rollback");
		let module = temporary.path().join("work/.git/modules/one");
		std::fs::create_dir_all(&module).unwrap();
		std::fs::write(module.join("config"), b"[core]\n\tbare = true\n").unwrap();
		let module_directory = context
			.open_git_subdir_nofollow(Path::new("modules/one"))
			.unwrap();
		let mount_plan = context.inspect_module_mount(&entry).await.unwrap();
		let marker = temporary.path().join("work/modules/one/.git");
		let configuration = MarkerRacingConfiguration {
			marker: marker.clone(),
			marker_bytes: b"foreign marker\n".to_vec(),
		};

		assert!(matches!(
			context
				.publish_module_mount(
					&entry,
					&mount_plan,
					&module_directory,
					true,
					&configuration,
					None,
				)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
		assert_eq!(std::fs::read(marker).unwrap(), b"foreign marker\n");
		assert_eq!(
			std::fs::read(module.join("config")).unwrap(),
			b"[core]\n\tbare = true\n"
		);
	}

	#[tokio::test]
	async fn same_content_marker_replacement_rolls_back_core_worktree() {
		let (temporary, context, entry) = mount_fixture("same-content-marker-race");
		let module = temporary.path().join("work/.git/modules/one");
		std::fs::create_dir_all(&module).unwrap();
		std::fs::write(module.join("config"), b"[core]\n\tbare = true\n").unwrap();
		let module_directory = context
			.open_git_subdir_nofollow(Path::new("modules/one"))
			.unwrap();
		let marker = temporary.path().join("work/modules/one/.git");
		std::fs::write(&marker, entry.pointers.marker.as_bytes()).unwrap();
		let mount_plan = context.inspect_module_mount(&entry).await.unwrap();
		let before_identity = match &mount_plan.marker {
			MarkerSnapshot::File { identity, .. } => *identity,
			MarkerSnapshot::Absent => panic!("fixture marker must exist"),
		};
		let configuration = MarkerRacingConfiguration {
			marker: marker.clone(),
			marker_bytes: entry.pointers.marker.as_bytes().to_vec(),
		};

		assert!(matches!(
			context
				.publish_module_mount(
					&entry,
					&mount_plan,
					&module_directory,
					false,
					&configuration,
					None,
				)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
		assert_eq!(
			std::fs::read(&marker).unwrap(),
			entry.pointers.marker.as_bytes()
		);
		assert_ne!(
			marker_identity(&mount_plan.directory, &marker, &entry.declaration.path).unwrap(),
			before_identity
		);
		assert_eq!(
			std::fs::read(module.join("config")).unwrap(),
			b"[core]\n\tbare = true\n"
		);
	}

	#[test]
	fn exclusive_new_marker_publish_never_replaces_an_occupied_name() {
		let temporary = tempfile::tempdir().unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		std::fs::write(temporary.path().join(".git"), b"foreign marker\n").unwrap();

		assert!(matches!(
			publish_new_mount_marker(
				&directory,
				b"gitdir: canonical\n",
				&temporary.path().join(".git"),
				"modules/one",
			),
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
		assert_eq!(
			std::fs::read(temporary.path().join(".git")).unwrap(),
			b"foreign marker\n"
		);
		let retired = std::fs::read_dir(temporary.path())
			.unwrap()
			.filter_map(Result::ok)
			.map(|entry| entry.path())
			.filter(|path| {
				path.file_name().is_some_and(|name| {
					name
						.to_string_lossy()
						.starts_with(".gitana-namespace-remove.")
				})
			})
			.collect::<Vec<_>>();
		if cfg!(windows) {
			assert!(retired.is_empty());
		} else {
			assert_eq!(retired.len(), 1);
			assert_eq!(std::fs::read(&retired[0]).unwrap(), b"gitdir: canonical\n");
		}
	}

	#[test]
	fn module_identity_rejects_namespace_replacement() {
		let (temporary, context, entry) = mount_fixture("module-identity");
		let module = temporary.path().join("work/.git/modules/one");
		std::fs::create_dir_all(&module).unwrap();
		let pinned = context
			.open_git_subdir_nofollow(Path::new("modules/one"))
			.unwrap();
		std::fs::rename(&module, temporary.path().join("work/.git/modules/retained")).unwrap();
		std::fs::create_dir(&module).unwrap();
		assert!(matches!(
			context.ensure_module_identity(&entry, &pinned),
			Err(SubmoduleError::InvalidRepository(name)) if name == "one"
		));
	}

	#[test]
	fn staged_repository_writes_stay_on_the_retained_directory() {
		let (temporary, context, _) = mount_fixture("staged-identity");
		let control = temporary.path().join("work/.git/gitana-submodule-update");
		let staged = control.join("repository");
		let retained = control.join("retained");
		std::fs::create_dir_all(&staged).unwrap();
		let pinned = context
			.open_git_subdir_nofollow(Path::new("gitana-submodule-update/repository"))
			.unwrap();

		std::fs::rename(&staged, &retained).unwrap();
		std::fs::create_dir(&staged).unwrap();
		pinned.write("owned", b"retained").unwrap();

		assert_eq!(std::fs::read(retained.join("owned")).unwrap(), b"retained");
		assert!(!staged.join("owned").exists());
		assert!(matches!(
			context.ensure_staged_identity("one", &pinned),
			Err(SubmoduleError::InvalidRepository(name)) if name == "one"
		));
	}

	#[test]
	fn directory_creation_flushes_each_new_components_parent() {
		let temporary = tempfile::tempdir().unwrap();
		let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let mut synced = Vec::new();
		ensure_directory_components(
			&root,
			Path::new("modules/nested/one"),
			temporary.path(),
			DirectoryNamespace::Mount("modules/nested/one"),
			|_, path| {
				synced.push(path.to_owned());
				Ok(())
			},
		)
		.unwrap();
		assert_eq!(
			synced,
			[
				temporary.path().to_owned(),
				temporary.path().join("modules"),
				temporary.path().join("modules/nested"),
			]
		);

		let mut repeated = Vec::new();
		ensure_directory_components(
			&root,
			Path::new("modules/nested/one"),
			temporary.path(),
			DirectoryNamespace::Mount("modules/nested/one"),
			|_, path| {
				repeated.push(path.to_owned());
				Ok(())
			},
		)
		.unwrap();
		assert!(repeated.is_empty());
	}

	#[test]
	fn repository_publication_flushes_destination_then_source_parent() {
		let mut synced = Vec::new();
		sync_repository_publication_parents(Path::new("modules/nested/one"), |path| {
			synced.push(path.to_owned());
			Ok(())
		})
		.unwrap();
		assert_eq!(
			synced,
			[
				PathBuf::from("modules/nested"),
				PathBuf::from("gitana-submodule-update"),
			]
		);
	}

	#[test]
	fn repository_publication_never_replaces_a_raced_empty_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let control_path = temporary.path().join("control");
		let modules_path = temporary.path().join("modules");
		std::fs::create_dir_all(control_path.join("repository")).unwrap();
		std::fs::write(control_path.join("repository/config"), b"prepared").unwrap();
		std::fs::create_dir_all(modules_path.join("one")).unwrap();
		let control = Dir::open_ambient_dir(&control_path, ambient_authority()).unwrap();
		let modules = Dir::open_ambient_dir(&modules_path, ambient_authority()).unwrap();

		assert!(rename_directory_noreplace(&control, "repository", &modules, "one".as_ref()).is_err());
		assert_eq!(
			std::fs::read(control_path.join("repository/config")).unwrap(),
			b"prepared"
		);
		assert_eq!(
			std::fs::read_dir(modules_path.join("one")).unwrap().count(),
			0
		);
	}

	#[cfg(not(windows))]
	#[test]
	fn pointer_rendering_preserves_a_literal_backslash() {
		assert_eq!(
			slash_path(Path::new("modules/one\\two")).unwrap(),
			"modules/one\\two"
		);
	}

	#[cfg(unix)]
	#[test]
	fn pointer_rendering_rejects_non_utf8_without_lossy_aliasing() {
		use std::ffi::OsString;
		use std::os::unix::ffi::OsStringExt;

		let path = std::path::PathBuf::from(OsString::from_vec(b"modules/one-\xff".to_vec()));
		assert!(matches!(
			slash_path(&path),
			Err(SubmoduleError::UnrepresentablePointerPath(rejected)) if rejected == path
		));
	}

	#[cfg(unix)]
	#[test]
	fn update_lock_remains_serialized_after_its_named_entry_is_replaced() {
		let (_temporary, first, _entry) = mount_fixture("replaced-update-lock");
		let second = reopen_context(&first);
		let first_guard = first.acquire_update_lock().unwrap();

		first
			.git
			.rename(UPDATE_LOCK, &first.git, "detached-update-lock")
			.unwrap();
		first.git.write(UPDATE_LOCK, b"replacement").unwrap();

		assert!(matches!(
			second.acquire_update_lock(),
			Err(SubmoduleError::UpdateLocked)
		));
		assert!(matches!(
			first_guard.validate(),
			Err(SubmoduleError::RecoveryRequired(message))
				if message.contains("lock entry changed")
		));
		assert_eq!(first.git.read(UPDATE_LOCK).unwrap(), b"replacement");

		drop(first_guard);
		let replacement_guard = second.acquire_update_lock().unwrap();
		replacement_guard.validate().unwrap();
	}

	#[cfg(windows)]
	#[test]
	fn update_lock_entry_cannot_be_detached_while_held() {
		let (_temporary, context, _entry) = mount_fixture("fixed-update-lock");
		let guard = context.acquire_update_lock().unwrap();

		assert!(
			context
				.git
				.rename(UPDATE_LOCK, &context.git, "detached-update-lock")
				.is_err()
		);
		assert!(context.git.remove_file(UPDATE_LOCK).is_err());
		guard.validate().unwrap();
	}

	#[cfg(unix)]
	fn reopen_context(context: &SubmoduleContext) -> SubmoduleContext {
		let worktree = context
			.layout
			.worktree_root
			.as_ref()
			.expect("fixture is a worktree");
		SubmoduleContext::new(
			context.layout.clone(),
			Dir::open_ambient_dir(&context.layout.common_dir, ambient_authority()).unwrap(),
			Dir::open_ambient_dir(&context.layout.git_dir, ambient_authority()).unwrap(),
			Dir::open_ambient_dir(worktree, ambient_authority()).unwrap(),
			context.configs.clone(),
			context.prefix.clone(),
			context.hash_kind,
		)
		.unwrap()
	}

	fn mount_fixture(name: &str) -> (tempfile::TempDir, SubmoduleContext, Planned<Sha256>) {
		let temporary = tempfile::Builder::new().prefix(name).tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(worktree.join("modules/one")).unwrap();
		std::fs::create_dir(&git_dir).unwrap();
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
			url: Some("source".to_owned()),
			branch: None,
			update: None,
		};
		let pointers = context.module_pointers(&declaration).unwrap();
		let entry = Planned {
			declaration,
			recorded: ObjectId::from_hex(&"0".repeat(64)).unwrap(),
			source_url: Some("source".to_owned()),
			state: None,
			recovering: false,
			intent_identity: None,
			pointers,
		};
		(temporary, context, entry)
	}
}
