use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, OpenOptions};
use gitana_config::GitConfig;
use gitana_file_store::{DurabilityTarget, FileStore};
use gitana_file_store_local::{CapWorkDir, LocalFileStore, WorkDirFs, same_directory_identity};
use gitana_fs_native::{
	EntryIdentity, directory_identity, file_identity, remove_dir_all_if_identity,
	remove_file_if_identity, rename_noreplace, rename_noreplace_if_identity, replace_if_identities,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::{HeadState, ReflogIntent, Repository, detect_hash_kind};
use gitana_worktree::WorkTree;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Digest};

use crate::context::{
	is_active, parse_marker_target, should_initialize_only_active, validate_update_strategy,
};
use crate::{
	ConfigurationProvider, FetchRepository, FetchSource, InitRequest, PrepareRepository,
	PrepareSource, RepositoryTransfer, SharedConfigGuard, SubmoduleContext, SubmoduleDeclaration,
	SubmoduleError, SubmoduleObjectId, SubmoduleUpdateTarget, UpdateFailure, UpdateMergeConflict,
	UpdateMergeOutcome, UpdateMergeResult, UpdateOutcome, UpdateOutcomeState, UpdateReport,
	UpdateRequest, UpdateStrategy, UpdateStrategyExecutor, declarations_by_path,
};

const CONTROL_DIR: &str = "gitana-submodule-update";
const INTENT_FILE: &str = "gitana-submodule-update/intent.json";
const INTENT_LOCK: &str = "gitana-submodule-update/intent.lock";
const STAGED_REPOSITORY: &str = "gitana-submodule-update/repository";
const INTENT_NAME: &str = "intent.json";
const INTENT_LOCK_NAME: &str = "intent.lock";
const STAGED_REPOSITORY_NAME: &str = "repository";
const UPDATE_LOCK: &str = "gitana-submodule-update.lock";
const SHARED_CONFIG_LOCK: &str = "gitana-submodule-config.lock";
const MARKER_TEMP_ATTEMPTS: u64 = 100;
static MARKER_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static CONTROL_RETIRE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum SharedConfigAccess {
	Setup,
	Mutation,
}

#[derive(Clone)]
struct Planned<H: HashAlgorithm> {
	declaration: SubmoduleDeclaration,
	/// Commit recorded by the superproject gitlink, retained for status/reporting.
	recorded: ObjectId<H>,
	target: SubmoduleUpdateTarget,
	/// Exact durable target selected by a prior interrupted invocation.
	recovery_target: Option<ObjectId<H>>,
	/// Whether successful population must retain the selected remote HEAD for no-fetch reuse.
	record_remote_head: bool,
	/// Full symbolic superproject branch captured for `branch = .`.
	superproject_branch: Option<String>,
	source_url: Option<String>,
	state: Option<UpdateOutcomeState>,
	strategy: Option<UpdateStrategy>,
	recovering: bool,
	intent_identity: Option<EntryIdentity>,
	module_config_lease: Option<crate::SubmoduleMutationLease>,
	/// Explicit depth applied to both new repositories and existing fetches.
	depth: Option<u32>,
	fetch: bool,
	/// Effective depth for initial repository creation, including `.gitmodules` recommendations.
	clone_depth: Option<u32>,
	pointers: ModulePointers,
}

struct UpdateExecution<'a, T, S> {
	transfer: &'a T,
	strategies: &'a S,
}

struct ExactRecoveryHint<H: HashAlgorithm> {
	name: String,
	path: String,
	target: ObjectId<H>,
	source_context: IntentSourceContext,
	record_remote_head: bool,
}

fn recommended_clone_depth(
	request: &UpdateRequest,
	declaration: &SubmoduleDeclaration,
) -> Option<u32> {
	request
		.depth
		.or_else(|| (request.recommend_shallow && declaration.shallow == Some(true)).then_some(1))
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

#[derive(Clone)]
pub(crate) struct ModulePointers {
	pub(crate) core_worktree: String,
	pub(crate) marker: String,
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
	ModuleLocal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StageIntent {
	version: u32,
	name: String,
	path: String,
	#[serde(default, skip_serializing_if = "String::is_empty")]
	recorded: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	gitlink: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	target: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	remote: Option<String>,
	source_fingerprint: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	source_context: Option<IntentSourceContext>,
	#[serde(default, skip_serializing_if = "is_false")]
	record_remote_head: bool,
}

fn is_false(value: &bool) -> bool {
	!*value
}

struct ConditionalFileCleanup {
	directory: Dir,
	name: &'static str,
	identity: EntryIdentity,
	armed: bool,
}

/// A submodule mutation lease tied to the per-worktree lock and, when needed, the common config lock.
///
/// The directory lock prevents a second Gitana invocation from entering the shared staging
/// namespace if an unrelated process detaches a named lock on Unix. Each entry identity still
/// detects namespace tampering so the current operation fails closed at its next boundary.
pub(crate) struct UpdateLockGuard {
	state: Arc<UpdateLockState>,
}

struct UpdateLockState {
	directory: Dir,
	identity: EntryIdentity,
	display_path: PathBuf,
	_directory_lock: Option<File>,
	_named_lock: File,
	shared_directory: Option<Dir>,
	shared_identity: Option<EntryIdentity>,
	shared_config_directory_identity: Option<EntryIdentity>,
	shared_display_path: Option<PathBuf>,
	shared_config_guard: Option<Arc<SharedConfigGuard>>,
	_shared_named_lock: Option<File>,
}

impl UpdateLockGuard {
	pub(crate) fn lease(&self) -> crate::SubmoduleMutationLease {
		match self.state.shared_config_directory_identity {
			Some(identity) => crate::SubmoduleMutationLease::retain_config_directory(
				Arc::clone(&self.state),
				identity,
				self.state.shared_config_guard.as_ref().map(Arc::clone),
			),
			None => crate::SubmoduleMutationLease::retain(Arc::clone(&self.state)),
		}
	}

	pub(crate) fn validate(&self) -> Result<(), SubmoduleError> {
		validate_lock_entry(
			&self.state.directory,
			UPDATE_LOCK,
			self.state.identity,
			&self.state.display_path,
			"submodule update lock entry changed while held",
		)?;
		if let (Some(directory), Some(identity), Some(display_path)) = (
			self.state.shared_directory.as_ref(),
			self.state.shared_identity,
			self.state.shared_display_path.as_ref(),
		) {
			validate_lock_entry(
				directory,
				SHARED_CONFIG_LOCK,
				identity,
				display_path,
				"shared submodule config lock entry changed while held",
			)?;
		}
		if let Some(guard) = &self.state.shared_config_guard {
			guard.validate()?;
		}
		Ok(())
	}
}

fn validate_lock_entry(
	directory: &Dir,
	name: &str,
	expected: EntryIdentity,
	display_path: &Path,
	changed: &str,
) -> Result<(), SubmoduleError> {
	let metadata = directory.symlink_metadata(name).map_err(|source| {
		if source.kind() == std::io::ErrorKind::NotFound {
			SubmoduleError::RecoveryRequired(changed.to_owned())
		} else {
			SubmoduleError::Io {
				path: display_path.to_owned(),
				source,
			}
		}
	})?;
	if !metadata.is_file()
		|| metadata.file_type().is_symlink()
		|| EntryIdentity::from_metadata(&metadata) != expected
	{
		return Err(SubmoduleError::RecoveryRequired(changed.to_owned()));
	}
	Ok(())
}

impl Drop for ConditionalFileCleanup {
	fn drop(&mut self) {
		if self.armed {
			let _ = remove_file_if_identity(&self.directory, OsStr::new(self.name), self.identity);
		}
	}
}

// `UpdateFailure` deliberately carries the completed update prefix alongside the structured source
// error. Boxing either public field would break the API for an error-path-only size optimization.
#[allow(clippy::result_large_err)]
impl SubmoduleContext {
	pub async fn update<
		C: ConfigurationProvider,
		T: RepositoryTransfer,
		S: UpdateStrategyExecutor,
	>(
		&self,
		request: &UpdateRequest,
		configuration: &C,
		transfer: &T,
		strategies: &S,
	) -> Result<UpdateReport, UpdateFailure> {
		match self.hash_kind {
			HashKind::Sha1 => {
				self
					.update_typed::<Sha1, C, T, S>(request, configuration, transfer, strategies, None)
					.await
			}
			HashKind::Sha256 => {
				self
					.update_typed::<Sha256, C, T, S>(request, configuration, transfer, strategies, None)
					.await
			}
		}
	}

	/// Resume this worktree's pending update transaction, if one exists.
	///
	/// The durable intent selects the recovery owner independently of a recursive caller's current
	/// path query. An empty control directory is retired directly under the update lock so recovery
	/// never broadens the caller's selection merely to reach the ordinary update state machine.
	#[allow(clippy::too_many_arguments)]
	pub async fn resume_pending_update<
		C: ConfigurationProvider,
		T: RepositoryTransfer,
		S: UpdateStrategyExecutor,
	>(
		&self,
		configuration: &C,
		transfer: &T,
		strategies: &S,
		reflog_committer: Option<String>,
		depth: Option<u32>,
		recommend_shallow: bool,
		remote: bool,
		fetch: bool,
	) -> Result<Option<UpdateReport>, UpdateFailure> {
		if depth == Some(0) {
			return Err(UpdateFailure::preflight(SubmoduleError::InvalidDepth));
		}
		self
			.ensure_no_repository_deinit_recovery()
			.map_err(UpdateFailure::preflight)?;
		let lock = self
			.acquire_update_lock()
			.map_err(UpdateFailure::preflight)?;
		let Some(query) = self
			.pending_update_query_locked(&lock)
			.map_err(UpdateFailure::preflight)?
		else {
			return Ok(None);
		};
		let request = UpdateRequest {
			query,
			initialize: false,
			depth,
			recommend_shallow,
			remote,
			fetch,
			strategy: None,
			initialize_only_active: false,
			reflog_committer,
		};
		let report = match self.hash_kind {
			HashKind::Sha1 => {
				self
					.update_typed::<Sha1, C, T, S>(&request, configuration, transfer, strategies, Some(lock))
					.await
			}
			HashKind::Sha256 => {
				self
					.update_typed::<Sha256, C, T, S>(
						&request,
						configuration,
						transfer,
						strategies,
						Some(lock),
					)
					.await
			}
		}?;
		Ok(Some(report))
	}

	/// Return the literal owner selection for this repository's pending update, if one exists.
	///
	/// An empty control directory is retired under the update lock. Callers can therefore use this
	/// operation to construct a recovery traversal without broadening it to every module.
	pub fn pending_update_query(&self) -> Result<Option<crate::SubmoduleQuery>, SubmoduleError> {
		if !repository_has_pending_update(&self.git, &self.layout.git_dir)? {
			return Ok(None);
		}
		let lock = self.acquire_update_lock()?;
		self.pending_update_query_locked(&lock)
	}

	fn pending_update_query_locked(
		&self,
		lock: &UpdateLockGuard,
	) -> Result<Option<crate::SubmoduleQuery>, SubmoduleError> {
		lock.validate()?;
		self.ensure_no_repository_deinit_recovery()?;
		if !repository_has_pending_update(&self.git, &self.layout.git_dir)? {
			return Ok(None);
		}
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		let Some((intent, _)) = read_stage_intent(&control, &self.layout.git_dir.join(INTENT_FILE))?
		else {
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
			lock.validate()?;
			return Ok(None);
		};
		if intent.path.is_empty() {
			return Err(SubmoduleError::RecoveryRequired(
				"staging intent has an empty path".to_owned(),
			));
		}
		let query = crate::SubmoduleQuery::top_literal(&intent.path);
		lock.validate()?;
		Ok(Some(query))
	}

	fn exact_recovery_hint<H: HashAlgorithm>(
		&self,
		declarations: &HashMap<String, SubmoduleDeclaration>,
	) -> Result<Option<ExactRecoveryHint<H>>, SubmoduleError> {
		if !repository_has_pending_update(&self.git, &self.layout.git_dir)? {
			return Ok(None);
		}
		let control = self
			.open_git_subdir_nofollow(Path::new(CONTROL_DIR))
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(CONTROL_DIR),
				source,
			})?;
		let Some((intent, _)) = read_stage_intent(&control, &self.layout.git_dir.join(INTENT_FILE))?
		else {
			return Ok(None);
		};
		let source_context = intent_source_context(&intent).ok_or_else(|| {
			let message = if matches!(intent.version, 1..=5) {
				format!(
					"staging intent version {} has an invalid source context",
					intent.version
				)
			} else {
				format!("unsupported staging intent version {}", intent.version)
			};
			SubmoduleError::RecoveryRequired(message)
		})?;
		let declaration = declarations
			.get(&intent.path)
			.filter(|declaration| declaration.name == intent.name)
			.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(format!("unfinished staging for '{}'", intent.name))
			})?;
		let module = Path::new("modules").join(&declaration.name);
		let published = safe_directory_exists(&self.git, &module, &self.layout.git_dir.join(&module))?;
		let staged = safe_directory_exists(
			&control,
			Path::new(STAGED_REPOSITORY_NAME),
			&self.layout.git_dir.join(STAGED_REPOSITORY),
		)?;
		if intent.version < 5 && !published && !staged {
			return Ok(None);
		}
		let target = intent_target(&intent)
			.ok_or_else(|| {
				SubmoduleError::RecoveryRequired("staging intent has no selected target".to_owned())
			})
			.and_then(|target| {
				ObjectId::from_hex(target).map_err(|_| {
					SubmoduleError::RecoveryRequired(
						"staging intent has an invalid selected target".to_owned(),
					)
				})
			})?;
		Ok(Some(ExactRecoveryHint {
			name: intent.name,
			path: intent.path,
			target,
			source_context,
			record_remote_head: intent.version == 5 && intent.record_remote_head,
		}))
	}

	async fn update_typed<
		H: HashAlgorithm,
		C: ConfigurationProvider,
		T: RepositoryTransfer,
		S: UpdateStrategyExecutor,
	>(
		&self,
		request: &UpdateRequest,
		configuration: &C,
		transfer: &T,
		strategies: &S,
		retained_lock: Option<UpdateLockGuard>,
	) -> Result<UpdateReport, UpdateFailure> {
		if request.depth == Some(0) {
			return Err(UpdateFailure::preflight(SubmoduleError::InvalidDepth));
		}
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
			if index.conflict(&crate::git_path(path)).is_some() {
				return Err(UpdateFailure::preflight(SubmoduleError::Conflicted(
					path.clone(),
				)));
			}
			if request.strategy.is_none()
				&& let Some(strategy) = declaration.update.as_deref()
			{
				validate_update_strategy(&declaration.name, strategy).map_err(UpdateFailure::preflight)?;
			}
			let module_pointers = self
				.module_pointers(declaration)
				.map_err(UpdateFailure::preflight)?;
			self
				.preflight_module_namespaces(declaration, &module_pointers, configuration)
				.await
				.map_err(UpdateFailure::preflight)?;
			pointers.insert(path.clone(), module_pointers);
		}
		let lock = if let Some(lock) = retained_lock {
			debug_assert!(
				!request.initialize,
				"recovery retains the plain update guard"
			);
			lock
		} else {
			if request.initialize {
				self.acquire_config_update_lock()
			} else {
				self.acquire_update_lock()
			}
			.map_err(UpdateFailure::preflight)?
		};
		lock.validate().map_err(UpdateFailure::preflight)?;
		let mutation_lease = lock.lease();
		if request.initialize {
			self
				.ensure_no_repository_deinit_recovery()
				.map_err(UpdateFailure::preflight)?;
		} else {
			self
				.ensure_no_deinit_recovery()
				.map_err(UpdateFailure::preflight)?;
		}
		let config_setup = if request.initialize {
			None
		} else {
			Some(
				self
					.acquire_config_setup_lease()
					.await
					.map_err(UpdateFailure::preflight)?,
			)
		};

		let initialized = if request.initialize {
			self
				.init_unlocked(
					&InitRequest {
						query: request.query.clone(),
					},
					configuration,
					request.initialize_only_active,
					request.strategy,
					mutation_lease.clone(),
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
		let effective = update_effective_config(configuration, &report).await?;
		if let Some(config_setup) = config_setup {
			config_setup
				.validate()
				.map_err(|source| UpdateFailure::after_init(&report, source))?;
		}
		let exact_recovery = self
			.exact_recovery_hint::<H>(&declarations)
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		let initialize_only_active =
			should_initialize_only_active(&request.query, &effective, request.initialize_only_active);
		let mut plan = Vec::with_capacity(selected.len());
		for path in selected {
			let declaration = declarations
				.get(&path)
				.expect("preflight established every mapping")
				.clone();
			let recorded = index
				.entry(&crate::git_path(&path))
				.expect("selected stage-zero gitlink")
				.oid;
			let recovery = exact_recovery
				.as_ref()
				.filter(|recovery| recovery.name == declaration.name && recovery.path == declaration.path);
			let active = if recovery.is_some() {
				true
			} else {
				is_active(&effective, &declaration.name, &declaration.path)
					.map_err(|source| UpdateFailure::after_init(&report, source))?
			};
			let strategy = if recovery.is_some() {
				Some(UpdateStrategy::Checkout)
			} else if let Some(strategy) = request.strategy {
				Some(strategy)
			} else {
				let strategy = configured_update_strategy(&effective, &declaration.name)
					.map_err(|source| UpdateFailure::after_init(&report, source))?
					.or_else(|| declaration.update.clone())
					.unwrap_or_else(|| "checkout".to_owned());
				parse_update_strategy(&declaration.name, &strategy)
					.map_err(|source| UpdateFailure::after_init(&report, source))?
			};
			if recovery.is_none() && initialize_only_active && !active {
				let clone_depth = recommended_clone_depth(request, &declaration);
				plan.push(Planned {
					declaration,
					recorded,
					target: SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(recorded)),
					recovery_target: None,
					record_remote_head: false,
					superproject_branch: None,
					source_url: None,
					state: Some(UpdateOutcomeState::SkippedInactive),
					strategy,
					recovering: false,
					intent_identity: None,
					module_config_lease: None,
					depth: request.depth,
					fetch: request.fetch,
					clone_depth,
					pointers: pointers
						.remove(&path)
						.expect("preflight computed every selected module pointer"),
				});
				continue;
			}
			let configured_url = if recovery
				.is_some_and(|recovery| recovery.source_context != IntentSourceContext::Superproject)
			{
				None
			} else {
				effective.get_raw("submodule", Some(&declaration.name), "url")
			};
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
			let module_relative = Path::new("modules").join(&declaration.name);
			let retained_repository = self
				.git
				.symlink_metadata(&module_relative)
				.is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
			let (registration_remote, module_config_lease) = if recovery.is_none()
				&& request.remote
				&& request.fetch
				&& source_url.is_none()
				&& retained_repository
			{
				let (remote, lease) = self
					.retained_module_remote::<H, C>(&declaration, configuration)
					.await
					.map_err(|source| UpdateFailure::after_init(&report, source))?;
				(Some(remote), Some(lease))
			} else {
				(None, None)
			};
			let state = if recovery.is_some() {
				None
			} else if source_url.is_none()
				&& ((request.fetch && registration_remote.as_deref() != Some(".")) || !retained_repository)
			{
				Some(UpdateOutcomeState::SkippedUnregistered)
			} else if !active {
				Some(UpdateOutcomeState::SkippedInactive)
			} else if strategy.is_none() {
				Some(UpdateOutcomeState::SkippedByStrategy)
			} else {
				None
			};
			let (target, recovery_target, superproject_branch) = if let Some(recovery) = recovery {
				(
					SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(recovery.target)),
					Some(recovery.target),
					None,
				)
			} else if state.is_none() {
				configured_update_target(
					request,
					&effective,
					&declaration,
					recorded,
					worktree.repository(),
				)
				.await
				.map(|(target, branch)| (target, None, branch))
				.map_err(|source| UpdateFailure::after_init(&report, source))?
			} else {
				(
					SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(recorded)),
					None,
					None,
				)
			};
			let clone_depth = recommended_clone_depth(request, &declaration);
			let record_remote_head = recovery.map_or_else(
				|| matches!(&target, SubmoduleUpdateTarget::RemoteHead),
				|recovery| recovery.record_remote_head,
			);
			plan.push(Planned {
				declaration,
				recorded,
				target,
				recovery_target,
				record_remote_head,
				superproject_branch,
				source_url,
				state,
				strategy,
				recovering: false,
				intent_identity: None,
				module_config_lease,
				depth: request.depth,
				fetch: request.fetch,
				clone_depth,
				pointers: pointers
					.remove(&path)
					.expect("preflight computed every selected module pointer"),
			});
		}

		lock
			.validate()
			.map_err(|source| UpdateFailure::after_init(&report, source))?;
		let recovery_index = self
			.recover_stage(&mut plan, &effective, configuration, transfer)
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
		let execution = UpdateExecution {
			transfer,
			strategies,
		};
		for entry in plan {
			lock.validate().map_err(|source| UpdateFailure {
				completed: report.clone(),
				module: Some(entry.declaration.name.clone()),
				source,
			})?;
			if let Some(state) = entry.state {
				report.outcomes.push(outcome(&entry, state, None, None));
				continue;
			}
			let module = entry.declaration.name.clone();
			let update = self
				.update_one(
					&entry,
					request.reflog_committer.as_deref(),
					&effective,
					configuration,
					&execution,
					&mutation_lease,
				)
				.await;
			match update {
				Ok((state, target, merge)) => {
					report
						.outcomes
						.push(outcome(&entry, state, Some(target), merge));
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

	async fn update_one<
		H: HashAlgorithm,
		C: ConfigurationProvider,
		T: RepositoryTransfer,
		S: UpdateStrategyExecutor,
	>(
		&self,
		entry: &Planned<H>,
		committer: Option<&str>,
		effective: &GitConfig,
		configuration: &C,
		execution: &UpdateExecution<'_, T, S>,
		mutation_lease: &crate::SubmoduleMutationLease,
	) -> Result<(UpdateOutcomeState, ObjectId<H>, Option<UpdateMergeOutcome>), SubmoduleError> {
		let mut intent_identity = entry.intent_identity;
		let mut selected_target = entry.recovery_target;
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
		let mount_before_transfer = self.inspect_module_mount(entry, configuration).await?;
		if !existing {
			let source = entry
				.source_url
				.as_ref()
				.ok_or_else(|| SubmoduleError::Unregistered(entry.declaration.name.clone()))?;
			let prepared = self
				.prepare_module(
					entry,
					source,
					effective,
					execution.transfer,
					mutation_lease.clone(),
				)
				.await?;
			intent_identity = Some(prepared.0);
			selected_target = Some(prepared.1);
			cloned = true;
		}
		let completion_required = cloned || entry.recovering || mount_before_transfer.newly_attached;
		let module_git_dir = self.layout.git_dir.join(&module_relative);
		let (mut repository, module_directory) =
			self.open_module_repository::<H>(&entry.declaration)?;
		// A retained module is independently addressable as a repository. Serialize its config from
		// the first read through attachment publication so a config command that already captured the
		// detached image cannot overwrite `core.worktree` after update reports success. The parent
		// guard is always acquired first; trying the module guard preserves that order without waiting
		// on an inverse acquisition in another process.
		let module_mutation_lease = match &entry.module_config_lease {
			Some(lease) => {
				if !lease
					.covers_config_directory(&module_directory)
					.map_err(|source| SubmoduleError::Io {
						path: module_git_dir.clone(),
						source,
					})? {
					return Err(SubmoduleError::InvalidRepository(
						entry.declaration.name.clone(),
					));
				}
				lease.clone()
			}
			None => try_acquire_submodule_config_mutation_lease(&module_directory, &module_git_dir)?,
		};
		if repository_has_pending_update(&module_directory, &module_git_dir)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending nested submodule update recovery in '{}' must be completed before updating its parent",
				entry.declaration.path
			)));
		}
		let module_layout = gitana_repository_layout::RepositoryLayout {
			worktree_root: mount_before_transfer
				.mounted
				.then(|| self.worktree_root().join(&entry.declaration.path)),
			git_dir: module_git_dir.clone(),
			common_dir: module_git_dir.clone(),
		};
		if crate::repository_has_pending_deinit(&module_directory, &module_directory, &module_layout)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending nested submodule deinit recovery in '{}' must be completed before updating its parent",
				entry.declaration.path
			)));
		}
		if crate::repository_has_pending_set_url_recovery(
			&module_directory,
			&module_directory,
			&module_layout,
		)? {
			return Err(SubmoduleError::RecoveryRequired(format!(
				"pending nested submodule set-url recovery in '{}' must be completed before updating its parent",
				entry.declaration.path
			)));
		}
		let mutation_lease = mutation_lease.clone().combine(module_mutation_lease);
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
		// A published repository accepted by `recover_stage` already has a durable selected object
		// graph and is source-bound by its matching intent. Recovery must therefore finish from that
		// local state even if the original source has disappeared. Ordinary existing repositories keep
		// Git's fetch-first update behavior, including retained repositories without a recovery intent.
		let mut resolved_existing_source = None;
		let mut existing_remote = None;
		let mut fetched_roots = Vec::new();
		if existing && !entry.recovering {
			let remote = if matches!(entry.target, SubmoduleUpdateTarget::Gitlink(_)) {
				"origin".to_owned()
			} else {
				module_update_remote(&repository, &config).await?
			};
			existing_remote = Some(remote.clone());
			if entry.fetch && remote != "." {
				let source = module_remote_url(&config, &entry.declaration.name, &remote)?;
				let transfer_directory =
					module_directory
						.try_clone()
						.map_err(|source| SubmoduleError::Io {
							path: module_git_dir.clone(),
							source,
						})?;
				let fetch_source = FetchSource {
					remote: remote.clone(),
					source_url: source,
					worktree_dir: self.worktree_root().join(&entry.declaration.path),
					config: config.clone(),
				};
				let resolved = execution
					.transfer
					.resolve_fetch_source_identity(&fetch_source)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				let fetched = Box::pin(execution.transfer.fetch_target(
					FetchRepository {
						source: fetch_source,
						git_dir: transfer_directory,
						display_git_dir: module_git_dir.clone(),
						hash_kind: crate::object_id::kind::<H>(),
						target: entry.target.clone(),
						depth: entry.depth,
					},
					mutation_lease.clone(),
				))
				.await
				.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				if fetched.resolved_source != resolved {
					return Err(SubmoduleError::Transfer(
						"submodule transfer source changed during fetch".to_owned(),
					));
				}
				selected_target = Some(typed_update_oid::<H>(&fetched.selected_target)?);
				resolved_existing_source = Some((fetched.resolved_source, IntentSourceContext::Module));
				fetched_roots = fetched.fetched_roots;
			} else {
				selected_target = Some(
					resolve_local_update_target(&repository, &entry.target, &remote, &entry.declaration.name)
						.await?,
				);
				resolved_existing_source = Some((
					local_source_identity(&remote, selected_target.unwrap()),
					IntentSourceContext::ModuleLocal,
				));
			}
		}
		let selected_target = selected_target.unwrap_or(entry.recorded);
		self.ensure_superproject_branch(entry).await?;
		if !repository.objects().exists_object(&selected_target).await? {
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		if entry.depth.is_some() && existing && !entry.recovering {
			let roots = durability_roots(selected_target, fetched_roots)?;
			repository
				.durability_barrier_object_graphs(&roots, &[])
				.await?;
		}
		let merge_existing = entry.strategy == Some(UpdateStrategy::Merge)
			&& existing
			&& !completion_required
			&& mount_before_transfer.mounted;
		let head_lock = if merge_existing {
			None
		} else {
			Some(repository.refs().lock_head().await?)
		};
		let current = repository.refs().resolve_head().await?;
		if !completion_required && mount_before_transfer.mounted && current.is_none() {
			return Err(SubmoduleError::UnbornModuleHead(
				entry.declaration.name.clone(),
			));
		}
		let needs_checkout =
			!merge_existing && (completion_required || current != Some(selected_target));
		if existing
			&& (merge_existing || needs_checkout)
			&& let Some(operation) = operation_in_progress(&repository).await?
		{
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
			.revalidate_module_mount(entry, &mount_before_transfer, configuration)
			.await?;
		self.ensure_superproject_branch(entry).await?;
		let resolved_attachment_source = if mount.newly_attached && !cloned && !entry.recovering {
			Some(resolved_existing_source.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"retained mount publication requires the fetched source identity".to_owned(),
				)
			})?)
		} else {
			None
		};
		if !merge_existing && !completion_required && mount.mounted && current == Some(selected_target)
		{
			let published_intent = self
				.publish_module_mount(
					entry,
					&mount,
					&module_directory,
					selected_target,
					false,
					(configuration, &mutation_lease),
					resolved_attachment_source
						.as_ref()
						.map(|(source, context)| (source.as_str(), *context)),
					existing_remote.as_deref(),
				)
				.await?;
			debug_assert!(published_intent.is_none());
			self.ensure_module_identity(entry, &module_directory)?;
			self.ensure_mount_identity(entry, &mount)?;
			self.ensure_mount_marker(entry, &mount)?;
			return Ok((UpdateOutcomeState::AlreadyCurrent, selected_target, None));
		}
		if merge_existing {
			let published_intent = self
				.publish_module_mount(
					entry,
					&mount,
					&module_directory,
					selected_target,
					false,
					(configuration, &mutation_lease),
					None,
					existing_remote.as_deref(),
				)
				.await?;
			debug_assert!(published_intent.is_none());
			self.ensure_module_identity(entry, &module_directory)?;
			self.ensure_mount_identity(entry, &mount)?;
			self.ensure_mount_marker(entry, &mount)?;
			let module_worktree_root = self.worktree_root().join(&entry.declaration.path);
			let work_directory = mount
				.directory
				.try_clone()
				.map_err(|source| SubmoduleError::Io {
					path: module_worktree_root.clone(),
					source,
				})?;
			let repository = module_repository_with_worker_keepalive::<H>(
				&module_directory,
				&module_git_dir,
				&config,
				mutation_lease.clone(),
			)?;
			let worktree = WorkTree::new_located(
				repository,
				CapWorkDir::from_dir(work_directory),
				self.layout.git_dir.join(&module_relative),
				module_worktree_root.clone(),
			);
			let worker_context = retained_merge_context(self)?;
			let worker_entry = entry.clone();
			let worker_mount =
				retained_mount_plan(&mount, &self.worktree_root().join(&entry.declaration.path))?;
			let worker_module_directory =
				module_directory
					.try_clone()
					.map_err(|source| SubmoduleError::Io {
						path: module_git_dir.clone(),
						source,
					})?;
			let worktree_directory =
				mount
					.directory
					.try_clone()
					.map_err(|source| SubmoduleError::Io {
						path: module_worktree_root.clone(),
						source,
					})?;
			let worker_lease = mutation_lease.clone();
			let strategy = execution.strategies.clone();
			let name = entry.declaration.name.clone();
			let merge_task = tokio::spawn(async move {
				validate_merge_authority(
					&worker_context,
					&worker_entry,
					&worker_mount,
					&worker_module_directory,
					&worker_lease,
				)?;
				let result = strategy
					.merge(
						name,
						worktree,
						module_worktree_root,
						worktree_directory,
						selected_target,
					)
					.await;
				finish_merge_after_validation(result, || {
					validate_merge_authority(
						&worker_context,
						&worker_entry,
						&worker_mount,
						&worker_module_directory,
						&worker_lease,
					)
				})
			});
			let result = await_retained_merge_task(merge_task).await?;
			return match result {
				UpdateMergeResult::Completed(outcome) => {
					Ok((UpdateOutcomeState::Merged, selected_target, Some(outcome)))
				}
				UpdateMergeResult::Conflict { paths } => Err(SubmoduleError::MergeConflict(Box::new(
					UpdateMergeConflict {
						name: entry.declaration.name.clone(),
						path: entry.declaration.path.clone(),
						target: SubmoduleObjectId::from_typed(selected_target),
						paths,
					},
				))),
			};
		}
		let target_tree = repository.commit_tree(selected_target).await?;
		let head_lock = head_lock.expect("checkout strategy retains the HEAD lock");
		let message = format!(
			"checkout: moving from {} to {}",
			current.map_or_else(|| "unborn".to_owned(), |oid| oid.to_hex()),
			selected_target
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
		let prepared_head = head_lock.prepare_detached(selected_target, reflog).await?;
		if let Some(published_intent) = self
			.publish_module_mount(
				entry,
				&mount,
				&module_directory,
				selected_target,
				cloned || entry.recovering,
				(configuration, &mutation_lease),
				resolved_attachment_source
					.as_ref()
					.map(|(source, context)| (source.as_str(), *context)),
				existing_remote.as_deref(),
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
		Ok((
			if cloned {
				UpdateOutcomeState::Cloned
			} else {
				UpdateOutcomeState::CheckedOut
			},
			selected_target,
			None,
		))
	}

	async fn prepare_module<H: HashAlgorithm, T: RepositoryTransfer>(
		&self,
		entry: &Planned<H>,
		source: &str,
		effective: &GitConfig,
		transfer: &T,
		mutation_lease: crate::SubmoduleMutationLease,
	) -> Result<(EntryIdentity, ObjectId<H>), SubmoduleError> {
		let request = PrepareSource {
			module_path: entry.declaration.path.clone(),
			declared_url: entry.declaration.url.clone(),
			source_url: source.to_owned(),
			persist_url: gitana_remote::redact_password(source),
			hash_kind: crate::object_id::kind::<H>(),
			target: entry.target.clone(),
			config: effective.clone(),
		};
		let resolved_source = transfer
			.resolve_source_identity(&request)
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		let prepared = transfer
			.prepare_source(request, mutation_lease)
			.await
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		if prepared.resolved_source != resolved_source {
			return Err(SubmoduleError::Transfer(
				"submodule transfer source changed during preparation".to_owned(),
			));
		}
		let target = typed_update_oid::<H>(&prepared.selected_target)?;
		let intent = stage_intent(
			entry,
			target,
			&prepared.resolved_source,
			IntentSourceContext::Superproject,
			None,
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
		let fetched_roots = transfer
			.populate_prepared(
				prepared.source,
				PrepareRepository {
					git_dir: transfer_directory,
					display_git_dir: self.layout.git_dir.join(STAGED_REPOSITORY),
					hash_kind: crate::object_id::kind::<H>(),
					target: SubmoduleObjectId::from_typed(target),
					record_remote_head: entry.record_remote_head,
					depth: entry.clone_depth,
				},
			)
			.await
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
		let durability_roots = durability_roots(target, fetched_roots)?;
		self
			.verify_staged::<H>(
				target,
				&durability_roots,
				&entry.declaration.name,
				&stage_directory,
			)
			.await?;
		let module_target = Path::new("modules").join(&entry.declaration.name);
		let target_parent = self.ensure_git_parent_directories(&module_target)?;
		let target_name = module_target
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
					path: self.layout.git_dir.join(&module_target),
					source,
				});
			}
		}
		let published = target_parent
			.open_dir_nofollow(target_name)
			.map_err(|source| SubmoduleError::Io {
				path: self.layout.git_dir.join(&module_target),
				source,
			})?;
		if !same_directory_identity(&stage_directory, &published).map_err(|source| {
			SubmoduleError::Io {
				path: self.layout.git_dir.join(&module_target),
				source,
			}
		})? {
			return Err(SubmoduleError::InvalidRepository(
				entry.declaration.name.clone(),
			));
		}
		sync_repository_publication_parents(&module_target, |parent| self.sync_git_directory(parent))?;
		Ok((intent_identity, target))
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
		fetched_roots: &[ObjectId<H>],
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
			.durability_barrier_object_graphs(fetched_roots, &[])
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
		effective: &GitConfig,
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
			let message = if matches!(intent.version, 1..=5) {
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
			entry.declaration.name == intent.name && entry.declaration.path == intent.path
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
		// A legacy intent with no repository name owns no durable content. Retire it before copying its
		// recorded object into the current plan: this invocation must retain its fresh gitlink or remote
		// target when it starts again from scratch.
		if !target_exists && !staged_exists && intent.version < 5 {
			self.clear_control_dir(Some(intent_identity))?;
			return Ok(None);
		}
		let gitlink_hex = intent_gitlink(&intent).ok_or_else(|| {
			SubmoduleError::RecoveryRequired("staging intent has no recorded gitlink".to_owned())
		})?;
		entry.recorded = ObjectId::<H>::from_hex(gitlink_hex).map_err(|_| {
			SubmoduleError::RecoveryRequired("staging intent has an invalid recorded gitlink".to_owned())
		})?;
		let target_hex = intent_target(&intent).ok_or_else(|| {
			SubmoduleError::RecoveryRequired("staging intent has no selected target".to_owned())
		})?;
		let selected_target = ObjectId::<H>::from_hex(target_hex).map_err(|_| {
			SubmoduleError::RecoveryRequired("staging intent has an invalid selected target".to_owned())
		})?;
		entry.recovery_target = Some(selected_target);
		entry.target = SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(selected_target));
		// Once either repository name exists, its resolved endpoint identity remains binding and recovery
		// must not silently reuse the repository after an `insteadOf` rule selects a different source.
		let (source, resolved, module_config_lease) = match source_context {
			IntentSourceContext::Superproject => {
				let source = entry.source_url.as_deref().ok_or_else(|| {
					SubmoduleError::RecoveryRequired(format!(
						"unfinished staging source does not match '{}'",
						intent.name
					))
				})?;
				let request = PrepareSource {
					module_path: entry.declaration.path.clone(),
					declared_url: entry.declaration.url.clone(),
					source_url: source.to_owned(),
					persist_url: gitana_remote::redact_password(source),
					hash_kind: crate::object_id::kind::<H>(),
					target: SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(selected_target)),
					config: effective.clone(),
				};
				let resolved = transfer
					.resolve_source_identity(&request)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				(source.to_owned(), resolved, None)
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
				// Module-scoped recovery derives its durable source binding from the retained
				// repository's own config. Serialize that first read and carry the same guard into
				// update completion so a writer cannot replace the validated image in between.
				let module_config_lease =
					try_acquire_submodule_config_mutation_lease(&module_directory, &module_git_dir)?;
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
				let remote = intent.remote.as_deref().unwrap_or("origin");
				let source = module_remote_url(&config, &entry.declaration.name, remote)?;
				let fetch_source = FetchSource {
					remote: remote.to_owned(),
					source_url: source.clone(),
					worktree_dir: self.worktree_root().join(&entry.declaration.path),
					config,
				};
				let resolved = transfer
					.resolve_fetch_source_identity(&fetch_source)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
				(source, resolved, Some(module_config_lease))
			}
			IntentSourceContext::ModuleLocal => {
				if !target_exists || staged_exists {
					return Err(SubmoduleError::RecoveryRequired(format!(
						"module-local staging requires one published repository for '{}'",
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
				let lease =
					try_acquire_submodule_config_mutation_lease(&module_directory, &module_git_dir)?;
				let remote = intent.remote.as_deref().unwrap_or("origin");
				let resolved = local_source_identity(remote, selected_target);
				(resolved.clone(), resolved, Some(lease))
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
		entry.module_config_lease = module_config_lease;
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

	async fn retained_module_remote<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		configuration: &C,
	) -> Result<(String, crate::SubmoduleMutationLease), SubmoduleError> {
		let module_git_dir = self.layout.git_dir.join("modules").join(&declaration.name);
		let (repository, module_directory) = self.open_module_repository::<H>(declaration)?;
		let lease = try_acquire_submodule_config_mutation_lease(&module_directory, &module_git_dir)?;
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
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
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
		let relative = Path::new("modules").join(&declaration.name);
		let current = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(declaration.name.clone()))?;
		if !same_directory_identity(&module_directory, &current).map_err(|source| {
			SubmoduleError::Io {
				path: module_git_dir,
				source,
			}
		})? {
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
		}
		let remote = module_update_remote(&repository, &config).await?;
		Ok((remote, lease))
	}

	pub(crate) fn module_pointers(
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

	pub(crate) async fn preflight_module_namespaces<C: ConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		pointers: &ModulePointers,
		configuration: &C,
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
		match work
			.lstat(&crate::git_path(".git"))
			.map_err(|source| SubmoduleError::Io {
				path: mount.join(".git"),
				source,
			})? {
			None => {
				if !work
					.read_dir(&crate::git_path(""))
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
				let current = work
					.read(&crate::git_path(".git"))
					.map_err(|source| SubmoduleError::Io {
						path: mount.join(".git"),
						source,
					})?;
				let equivalent = if current == pointers.marker.as_bytes() {
					true
				} else if let Some(target) = std::str::from_utf8(&current)
					.ok()
					.and_then(parse_marker_target)
				{
					self
						.marker_targets_expected(declaration, target, configuration)
						.await?
				} else {
					false
				};
				if !equivalent {
					return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
				}
			}
			Some(_) => return Err(SubmoduleError::ForeignMount(declaration.path.clone())),
		}
		Ok(())
	}

	async fn inspect_module_mount<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned<H>,
		configuration: &C,
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
		let marker = crate::git_path(".git");
		let (mounted, newly_attached, marker_snapshot) =
			match work.lstat(&marker).map_err(|source| SubmoduleError::Io {
				path: mount.join(".git"),
				source,
			})? {
				None => {
					if !work
						.read_dir(&crate::git_path(""))
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
						.read_path_versioned(".git")
						.await
						.map_err(gitana_object_store::ObjectStoreError::from)?;
					if marker_identity(&work_directory, &mount.join(".git"), &declaration.path)? != identity {
						return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
					}
					if current != entry.pointers.marker.as_bytes() {
						let equivalent = if let Some(target) = std::str::from_utf8(&current)
							.ok()
							.and_then(parse_marker_target)
						{
							self
								.marker_targets_expected(declaration, target, configuration)
								.await?
						} else {
							false
						};
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

	async fn revalidate_module_mount<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned<H>,
		previous: &MountPlan,
		configuration: &C,
	) -> Result<MountPlan, SubmoduleError> {
		let current = self.inspect_module_mount(entry, configuration).await?;
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
		let marker = work
			.read(&crate::git_path(".git"))
			.map_err(|source| SubmoduleError::Io {
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
		let marker = work
			.read(&crate::git_path(".git"))
			.map_err(|source| SubmoduleError::Io {
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

	#[allow(clippy::too_many_arguments)]
	async fn publish_module_mount<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
		module_directory: &Dir,
		selected_target: ObjectId<H>,
		intent_durable: bool,
		configuration_and_lease: (&C, &crate::SubmoduleMutationLease),
		resolved_source: Option<(&str, IntentSourceContext)>,
		remote: Option<&str>,
	) -> Result<Option<EntryIdentity>, SubmoduleError> {
		let (configuration, mutation_lease) = configuration_and_lease;
		let declaration = &entry.declaration;
		let published_intent = if mount_plan.newly_attached && !intent_durable {
			let (resolved, context) = resolved_source.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"new mount publication requires a resolved source identity".to_owned(),
				)
			})?;
			Some(self.prepare_control_dir(&stage_intent(
				entry,
				selected_target,
				resolved,
				context,
				remote,
			))?)
		} else {
			None
		};

		// Revalidate the complete marker/emptiness snapshot immediately before the only repository
		// mutation in this publication step. A later marker race is still possible, so the config
		// edit below is paired with a conditional rollback token.
		self
			.revalidate_module_mount(entry, mount_plan, configuration)
			.await?;
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
			.set_module_worktree(
				edit_directory,
				&config_path,
				&entry.pointers.core_worktree,
				mutation_lease.clone(),
			)
			.await?;
		let publication = async {
			self.ensure_module_identity(entry, module_directory)?;
			self
				.publish_mount_marker(entry, mount_plan, configuration)
				.await
		}
		.await;
		if let Err(error) = publication {
			if let Err(rollback) = configuration
				.rollback_module_worktree(
					rollback_directory,
					&config_path,
					edit,
					mutation_lease.clone(),
				)
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

	async fn publish_mount_marker<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		entry: &Planned<H>,
		mount_plan: &MountPlan,
		configuration: &C,
	) -> Result<(), SubmoduleError> {
		let declaration = &entry.declaration;
		self
			.revalidate_module_mount(entry, mount_plan, configuration)
			.await?;
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

	async fn ensure_superproject_branch<H: HashAlgorithm>(
		&self,
		entry: &Planned<H>,
	) -> Result<(), SubmoduleError> {
		let Some(expected) = entry.superproject_branch.as_deref() else {
			return Ok(());
		};
		let repository = self.worktree::<H>()?;
		if repository.repository().refs().read_head().await? != HeadState::Symbolic(expected.to_owned())
		{
			return Err(SubmoduleError::RecoveryRequired(format!(
				"superproject branch changed while selecting remote target for '{}'",
				entry.declaration.name
			)));
		}
		Ok(())
	}

	fn ensure_empty_mount(&self, path: &str) -> Result<(), SubmoduleError> {
		self.ensure_mount_directory(path, true)
	}

	pub(crate) fn ensure_mount_directory(
		&self,
		path: &str,
		require_empty: bool,
	) -> Result<(), SubmoduleError> {
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

	pub(crate) fn acquire_update_lock(&self) -> Result<UpdateLockGuard, SubmoduleError> {
		acquire_update_lock(&self.git, &self.layout.git_dir)
	}

	pub(crate) fn acquire_config_update_lock(&self) -> Result<UpdateLockGuard, SubmoduleError> {
		acquire_update_lock_with_common(
			&self.git,
			&self.layout.git_dir,
			&self.common,
			&self.layout.common_dir,
		)
	}
}

fn module_repository_with_worker_keepalive<H: HashAlgorithm>(
	module_directory: &Dir,
	module_git_dir: &Path,
	config: &GitConfig,
	mutation_lease: crate::SubmoduleMutationLease,
) -> Result<Repository<LocalFileStore, H>, SubmoduleError> {
	let files = LocalFileStore::from_dir(module_directory.try_clone().map_err(|source| {
		SubmoduleError::Io {
			path: module_git_dir.to_owned(),
			source,
		}
	})?)
	.with_worker_keepalive(Arc::new(mutation_lease));
	let mut repository = Repository::new(ObjectStore::new(files));
	repository.set_effective_config(config.clone());
	Ok(repository)
}

fn retained_merge_context(context: &SubmoduleContext) -> Result<SubmoduleContext, SubmoduleError> {
	SubmoduleContext::new(
		context.layout.clone(),
		context.clone_dir(&context.common, &context.layout.common_dir)?,
		context.clone_dir(&context.git, &context.layout.git_dir)?,
		context.clone_dir(&context.work, context.worktree_root())?,
		context.configs.clone(),
		context.prefix.clone(),
		context.hash_kind,
	)
}

fn retained_mount_plan(mount: &MountPlan, path: &Path) -> Result<MountPlan, SubmoduleError> {
	Ok(MountPlan {
		directory: mount
			.directory
			.try_clone()
			.map_err(|source| SubmoduleError::Io {
				path: path.to_owned(),
				source,
			})?,
		mounted: mount.mounted,
		newly_attached: mount.newly_attached,
		marker: mount.marker.clone(),
	})
}

fn validate_merge_authority<H: HashAlgorithm>(
	context: &SubmoduleContext,
	entry: &Planned<H>,
	mount: &MountPlan,
	module_directory: &Dir,
	lease: &crate::SubmoduleMutationLease,
) -> Result<(), SubmoduleError> {
	lease.validate()?;
	context.ensure_mount_identity(entry, mount)?;
	context.ensure_mount_marker(entry, mount)?;
	context.ensure_module_identity(entry, module_directory)
}

fn finish_merge_after_validation(
	result: Result<UpdateMergeResult, SubmoduleError>,
	validate: impl FnOnce() -> Result<(), SubmoduleError>,
) -> Result<UpdateMergeResult, SubmoduleError> {
	validate()?;
	result
}

async fn await_retained_merge_task(
	task: tokio::task::JoinHandle<Result<UpdateMergeResult, SubmoduleError>>,
) -> Result<UpdateMergeResult, SubmoduleError> {
	task.await.map_err(|error| {
		SubmoduleError::Merge(format!("retained submodule merge worker failed: {error}"))
	})?
}

#[allow(clippy::result_large_err)] // See the `SubmoduleContext` implementation above.
async fn update_effective_config<C: ConfigurationProvider>(
	configuration: &C,
	report: &UpdateReport,
) -> Result<GitConfig, UpdateFailure> {
	// The caller holds either the common config mutation lock (`--init`) or a fresh setup lease
	// acquired after the per-worktree update lock (plain update). Rebuild the effective view from
	// that serialized state so an intervening deinit cannot be undone from the command-setup image.
	configuration
		.reload()
		.await
		.map_err(|source| UpdateFailure::after_init(report, source))
}

pub(crate) fn acquire_update_lock(
	git: &Dir,
	git_dir: &Path,
) -> Result<UpdateLockGuard, SubmoduleError> {
	acquire_update_lock_inner(git, git_dir, false)
}

fn acquire_update_lock_inner(
	git: &Dir,
	git_dir: &Path,
	wait: bool,
) -> Result<UpdateLockGuard, SubmoduleError> {
	let path = git_dir.join(UPDATE_LOCK);
	#[cfg(unix)]
	let directory_lock = {
		let mut options = OpenOptions::new();
		options.read(true);
		let directory = git
			.open_with(".", &options)
			.map_err(|source| SubmoduleError::Io {
				path: git_dir.to_owned(),
				source,
			})?
			.into_std();
		lock_file(&directory, git_dir, wait)?;
		Some(directory)
	};
	#[cfg(not(unix))]
	let directory_lock = None;
	match git.symlink_metadata(UPDATE_LOCK) {
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
	let file = git
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
	lock_file(&named_lock, &path, wait)?;
	let state = UpdateLockState {
		directory: git.try_clone().map_err(|source| SubmoduleError::Io {
			path: git_dir.to_owned(),
			source,
		})?,
		identity,
		display_path: path,
		_directory_lock: directory_lock,
		_named_lock: named_lock,
		shared_directory: None,
		shared_identity: None,
		shared_config_directory_identity: None,
		shared_display_path: None,
		shared_config_guard: None,
		_shared_named_lock: None,
	};
	let guard = UpdateLockGuard {
		state: Arc::new(state),
	};
	guard.validate()?;
	Ok(guard)
}

pub(crate) fn acquire_update_lock_with_common(
	git: &Dir,
	git_dir: &Path,
	common: &Dir,
	common_dir: &Path,
) -> Result<UpdateLockGuard, SubmoduleError> {
	acquire_update_lock_with_common_inner(git, git_dir, common, common_dir, false)
}

fn acquire_update_lock_with_common_inner(
	git: &Dir,
	git_dir: &Path,
	common: &Dir,
	common_dir: &Path,
	wait: bool,
) -> Result<UpdateLockGuard, SubmoduleError> {
	let mut guard = acquire_update_lock_inner(git, git_dir, wait)?;
	let config_directory_identity =
		directory_identity(common).map_err(|source| SubmoduleError::Io {
			path: common_dir.to_owned(),
			source,
		})?;
	let (shared_directory, identity, path, shared_config_guard, named_lock) =
		acquire_shared_config_mutation_lock(common, common_dir, wait)?;
	let state = Arc::get_mut(&mut guard.state).expect("new update lock guard is uniquely owned");
	state.shared_directory = Some(shared_directory);
	state.shared_identity = Some(identity);
	state.shared_config_directory_identity = Some(config_directory_identity);
	state.shared_display_path = Some(path);
	state.shared_config_guard = Some(shared_config_guard);
	state._shared_named_lock = Some(named_lock);
	guard.validate()?;
	Ok(guard)
}

fn acquire_shared_config_mutation_lock(
	common: &Dir,
	common_dir: &Path,
	wait: bool,
) -> Result<(Dir, EntryIdentity, PathBuf, Arc<SharedConfigGuard>, File), SubmoduleError> {
	let config_guard =
		lock_shared_config_guard(common, common_dir, wait, SharedConfigAccess::Mutation)?;
	let path = common_dir.join(SHARED_CONFIG_LOCK);
	match common.symlink_metadata(SHARED_CONFIG_LOCK) {
		Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
		Ok(_) => {
			return Err(SubmoduleError::RecoveryRequired(
				"shared submodule config lock is not a regular file".to_owned(),
			));
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(source) => return Err(SubmoduleError::Io { path, source }),
	}
	let mut options = OpenOptions::new();
	options.read(true).write(true).create(true);
	options.follow(FollowSymlinks::No);
	#[cfg(windows)]
	{
		use cap_std::fs::OpenOptionsExt as _;
		use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
		options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
	}
	let file = common
		.open_with(SHARED_CONFIG_LOCK, &options)
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
			"shared submodule config lock is not a regular file".to_owned(),
		));
	}
	let identity = file_identity(&file).map_err(|source| SubmoduleError::Io {
		path: path.clone(),
		source,
	})?;
	let named_lock = file.into_std();
	lock_file(&named_lock, &path, wait)?;
	let directory = common.try_clone().map_err(|source| SubmoduleError::Io {
		path: common_dir.to_owned(),
		source,
	})?;
	validate_lock_entry(
		&directory,
		SHARED_CONFIG_LOCK,
		identity,
		&path,
		"shared submodule config lock entry changed while held",
	)?;
	Ok((directory, identity, path, config_guard, named_lock))
}

fn lock_file(file: &File, path: &Path, wait: bool) -> Result<(), SubmoduleError> {
	if wait {
		File::lock(file).map_err(|source| SubmoduleError::Io {
			path: path.to_owned(),
			source,
		})
	} else {
		File::try_lock(file).map_err(|error| match error {
			std::fs::TryLockError::WouldBlock => SubmoduleError::UpdateLocked,
			std::fs::TryLockError::Error(source) => SubmoduleError::Io {
				path: path.to_owned(),
				source,
			},
		})
	}
}

/// Lock a repository-owned inode distinct from every per-worktree Git directory.
///
/// Unix needs a stable guard in addition to the replaceable named lock. The common refs directory
/// is shared by every linked worktree but remains distinct for repositories that share only object
/// storage, and it does not alias the main worktree's long-lived update guard. Opening it through the
/// common capability follows the same supported layout as repository discovery, while the retained
/// file handle pins the resolved directory inode for this operation.
fn lock_shared_config_guard(
	common: &Dir,
	common_dir: &Path,
	wait: bool,
	access: SharedConfigAccess,
) -> Result<Arc<SharedConfigGuard>, SubmoduleError> {
	let path = common_dir.join("refs");
	let metadata = common
		.symlink_metadata("refs")
		.map_err(|source| SubmoduleError::Io {
			path: path.clone(),
			source,
		})?;
	if !metadata.is_dir() && !metadata.file_type().is_symlink() {
		return Err(SubmoduleError::RecoveryRequired(
			"shared config guard is not a directory".to_owned(),
		));
	}
	let entry_identity = EntryIdentity::from_metadata(&metadata);
	let refs = common
		.open_dir("refs")
		.map_err(|source| SubmoduleError::Io {
			path: path.clone(),
			source,
		})?;
	let target_identity = directory_identity(&refs).map_err(|source| SubmoduleError::Io {
		path: path.clone(),
		source,
	})?;
	if gitana_fs_native::entry_identity(common, OsStr::new("refs")).map_err(|source| {
		SubmoduleError::Io {
			path: path.clone(),
			source,
		}
	})? != entry_identity
	{
		return Err(SubmoduleError::RecoveryRequired(
			"shared config guard changed while opening".to_owned(),
		));
	}
	#[cfg(unix)]
	let lock = {
		let mut options = OpenOptions::new();
		options.read(true);
		let directory = refs
			.open_with(".", &options)
			.map_err(|source| SubmoduleError::Io {
				path: path.clone(),
				source,
			})?
			.into_std();
		match access {
			SharedConfigAccess::Setup => lock_file_shared(&directory, &path, wait)?,
			SharedConfigAccess::Mutation => lock_file(&directory, &path, wait)?,
		}
		Some(directory)
	};
	#[cfg(windows)]
	let lock = {
		use std::time::Duration;

		use cap_std::fs::OpenOptionsExt as _;
		use windows_sys::Win32::Storage::FileSystem::{
			DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
			FILE_SHARE_READ, FILE_SHARE_WRITE,
		};

		loop {
			let mut options = OpenOptions::new();
			let desired = FILE_READ_ATTRIBUTES
				| FILE_LIST_DIRECTORY
				| if matches!(access, SharedConfigAccess::Mutation) {
					DELETE
				} else {
					0
				};
			let sharing = FILE_SHARE_READ | FILE_SHARE_WRITE;
			options
				.access_mode(desired)
				.share_mode(sharing)
				.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
				.follow(FollowSymlinks::No);
			match refs.open_with(".", &options) {
				Ok(directory) => break Some(directory.into_std()),
				Err(source) if windows_lock_contended(&source) && wait => {
					std::thread::sleep(Duration::from_millis(1));
				}
				Err(source) if windows_lock_contended(&source) => {
					return Err(SubmoduleError::UpdateLocked);
				}
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: path.clone(),
						source,
					});
				}
			}
		}
	};
	#[cfg(not(any(unix, windows)))]
	let lock = {
		let _ = (common, common_dir, wait, access);
		None
	};
	let guard = Arc::new(SharedConfigGuard::new(
		common.try_clone().map_err(|source| SubmoduleError::Io {
			path: common_dir.to_owned(),
			source,
		})?,
		refs,
		entry_identity,
		target_identity,
		path,
		lock,
	));
	guard.validate()?;
	Ok(guard)
}

#[cfg(windows)]
fn windows_lock_contended(error: &std::io::Error) -> bool {
	matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(unix)]
fn lock_file_shared(file: &File, path: &Path, wait: bool) -> Result<(), SubmoduleError> {
	if wait {
		File::lock_shared(file).map_err(|source| SubmoduleError::Io {
			path: path.to_owned(),
			source,
		})
	} else {
		File::try_lock_shared(file).map_err(|error| match error {
			std::fs::TryLockError::WouldBlock => SubmoduleError::UpdateLocked,
			std::fs::TryLockError::Error(source) => SubmoduleError::Io {
				path: path.to_owned(),
				source,
			},
		})
	}
}

/// Acquire the common submodule config lock for a repository command's configuration setup reads.
///
/// This waits for an active mutation rather than exposing its temporary Windows config displacement.
/// Because waiting is blocking, native frontends must invoke it from a blocking worker.
pub fn acquire_submodule_config_setup_lease(
	common: &Dir,
	common_dir: &Path,
) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
	let directory_lock =
		lock_shared_config_guard(common, common_dir, true, SharedConfigAccess::Setup)?;
	let identity = directory_identity(common).map_err(|source| SubmoduleError::Io {
		path: common_dir.to_owned(),
		source,
	})?;
	Ok(crate::SubmoduleMutationLease::retain_config_directory(
		Arc::clone(&directory_lock),
		identity,
		Some(directory_lock),
	))
}

/// Try to acquire common-config serialization for a repository command's configuration reads.
///
/// This is the nonblocking counterpart to [`acquire_submodule_config_setup_lease`]. Callers that
/// already retain an unrelated repository mutation lease use it to avoid introducing a
/// cross-repository lock cycle; contention is reported as [`SubmoduleError::UpdateLocked`].
pub fn try_acquire_submodule_config_setup_lease(
	common: &Dir,
	common_dir: &Path,
) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
	let directory_lock =
		lock_shared_config_guard(common, common_dir, false, SharedConfigAccess::Setup)?;
	let identity = directory_identity(common).map_err(|source| SubmoduleError::Io {
		path: common_dir.to_owned(),
		source,
	})?;
	Ok(crate::SubmoduleMutationLease::retain_config_directory(
		Arc::clone(&directory_lock),
		identity,
		Some(directory_lock),
	))
}

/// Acquire exclusive common-config serialization for a frontend-owned repository config mutation.
///
/// Unlike [`acquire_submodule_config_setup_lease`], this may create the repository-owned named lock
/// and must therefore be used only by commands that are already authorized to mutate configuration.
pub fn acquire_submodule_config_mutation_lease(
	common: &Dir,
	common_dir: &Path,
) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
	let identity = directory_identity(common).map_err(|source| SubmoduleError::Io {
		path: common_dir.to_owned(),
		source,
	})?;
	let retained = acquire_shared_config_mutation_lock(common, common_dir, true)?;
	let guard = Arc::clone(&retained.3);
	Ok(crate::SubmoduleMutationLease::retain_config_directory(
		Arc::new(retained),
		identity,
		Some(guard),
	))
}

/// Try to acquire exclusive config serialization without waiting.
///
/// Deinit uses this after acquiring the superproject guard so lock ordering stays stable and an
/// already-running module config writer is reported before any deinit namespace is changed.
pub(crate) fn try_acquire_submodule_config_mutation_lease(
	common: &Dir,
	common_dir: &Path,
) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
	let identity = directory_identity(common).map_err(|source| SubmoduleError::Io {
		path: common_dir.to_owned(),
		source,
	})?;
	let retained = acquire_shared_config_mutation_lock(common, common_dir, false)?;
	let guard = Arc::clone(&retained.3);
	Ok(crate::SubmoduleMutationLease::retain_config_directory(
		Arc::new(retained),
		identity,
		Some(guard),
	))
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

fn parse_update_strategy(
	name: &str,
	strategy: &str,
) -> Result<Option<UpdateStrategy>, SubmoduleError> {
	match strategy {
		"checkout" => Ok(Some(UpdateStrategy::Checkout)),
		"merge" => Ok(Some(UpdateStrategy::Merge)),
		"none" => Ok(None),
		_ => Err(SubmoduleError::UnsupportedStrategy {
			name: name.to_owned(),
			strategy: strategy.to_owned(),
		}),
	}
}

async fn configured_update_target<F: FileStore, H: HashAlgorithm>(
	request: &UpdateRequest,
	config: &GitConfig,
	declaration: &SubmoduleDeclaration,
	recorded: ObjectId<H>,
	repository: &Repository<F, H>,
) -> Result<(SubmoduleUpdateTarget, Option<String>), SubmoduleError> {
	if !request.remote {
		return Ok((
			SubmoduleUpdateTarget::Gitlink(SubmoduleObjectId::from_typed(recorded)),
			None,
		));
	}
	let configured = match config.get_raw("submodule", Some(&declaration.name), "branch") {
		Some(Some(branch)) if !branch.is_empty() => Some(branch.to_owned()),
		Some(_) => {
			return Err(SubmoduleError::MissingValue(format!(
				"submodule.{}.branch",
				declaration.name
			)));
		}
		None => declaration.branch.clone(),
	};
	let Some(branch) = configured else {
		return Ok((SubmoduleUpdateTarget::RemoteHead, None));
	};
	if branch != "." {
		validate_ref_fragment(&branch, "submodule branch")?;
		return Ok((SubmoduleUpdateTarget::RemoteBranch(branch), None));
	}
	let HeadState::Symbolic(symbolic) = repository.refs().read_head().await? else {
		return Err(SubmoduleError::DetachedSuperproject(
			declaration.name.clone(),
		));
	};
	let branch = symbolic
		.strip_prefix("refs/heads/")
		.ok_or_else(|| SubmoduleError::DetachedSuperproject(declaration.name.clone()))?;
	validate_ref_fragment(branch, "superproject branch")?;
	Ok((
		SubmoduleUpdateTarget::RemoteBranch(branch.to_owned()),
		Some(symbolic),
	))
}

fn validate_ref_fragment(value: &str, kind: &str) -> Result<(), SubmoduleError> {
	let invalid = value.is_empty()
		|| value.starts_with('/')
		|| value.ends_with('/')
		|| value.ends_with('.')
		|| value.contains("..")
		|| value.contains("@{")
		|| value.contains("//")
		|| value
			.split('/')
			.any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
		|| value.bytes().any(|byte| {
			byte <= b' '
				|| byte == 0x7f
				|| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
		});
	if invalid {
		return Err(SubmoduleError::Transfer(format!(
			"invalid {kind} '{value}'"
		)));
	}
	Ok(())
}

#[cfg(test)]
fn module_origin_url(
	config: &gitana_config::GitConfig,
	module: &str,
) -> Result<String, SubmoduleError> {
	crate::remote_url::first_fetch_url(config, "origin")?
		.map(str::to_owned)
		.ok_or_else(|| SubmoduleError::MissingModuleOrigin(module.to_owned()))
}

pub(crate) async fn module_update_remote<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	config: &GitConfig,
) -> Result<String, SubmoduleError> {
	let HeadState::Symbolic(head) = repository.refs().read_head().await? else {
		return Ok("origin".to_owned());
	};
	let Some(branch) = head.strip_prefix("refs/heads/") else {
		return Ok("origin".to_owned());
	};
	let remote = match config.get_raw("branch", Some(branch), "remote") {
		Some(Some(remote)) if !remote.is_empty() => remote.to_owned(),
		Some(_) => {
			return Err(SubmoduleError::MissingValue(format!(
				"branch.{branch}.remote"
			)));
		}
		None => "origin".to_owned(),
	};
	if remote != "." {
		gitana_remote::validate_remote_name(&remote)
			.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
	}
	Ok(remote)
}

fn module_remote_url(
	config: &GitConfig,
	module: &str,
	remote: &str,
) -> Result<String, SubmoduleError> {
	if remote == "." {
		return Ok(".".to_owned());
	}
	crate::remote_url::first_fetch_url(config, remote)?
		.map(str::to_owned)
		.ok_or_else(|| SubmoduleError::MissingModuleRemote {
			name: module.to_owned(),
			remote: remote.to_owned(),
		})
}

async fn resolve_local_update_target<F: FileStore, H: HashAlgorithm>(
	repository: &Repository<F, H>,
	target: &SubmoduleUpdateTarget,
	remote: &str,
	module: &str,
) -> Result<ObjectId<H>, SubmoduleError> {
	let (name, oid) = match target {
		SubmoduleUpdateTarget::Gitlink(oid) => {
			return typed_update_oid::<H>(oid);
		}
		SubmoduleUpdateTarget::RemoteHead if remote == "." => {
			("HEAD".to_owned(), repository.refs().resolve_head().await?)
		}
		SubmoduleUpdateTarget::RemoteHead => {
			let name = format!("refs/remotes/{remote}/HEAD");
			let oid = repository.refs().resolve_symbolic_exact(&name).await?;
			(name, oid)
		}
		SubmoduleUpdateTarget::RemoteBranch(branch) if remote == "." => {
			let name = format!("refs/heads/{branch}");
			let oid = repository.refs().resolve_symbolic_exact(&name).await?;
			(name, oid)
		}
		SubmoduleUpdateTarget::RemoteBranch(branch) => {
			let source = format!("refs/heads/{branch}");
			let config = repository.effective_config().await?;
			let name = gitana_remote::effective_fetch_destination(&config, remote, &source)
				.map_err(|error| SubmoduleError::Transfer(error.to_string()))?;
			let remote_head = format!("refs/remotes/{remote}/HEAD");
			if name.is_none()
				&& gitana_remote::effective_fetch_refspecs(&config, remote)
					.map_err(|error| SubmoduleError::Transfer(error.to_string()))?
					.iter()
					.any(|spec| {
						spec.exact_source().is_none()
							&& spec.destination(&source).as_deref() == Some(remote_head.as_str())
					}) {
				return Err(SubmoduleError::Transfer(format!(
					"remote '{remote}' branch '{branch}' maps to reserved remote HEAD ref '{remote_head}'"
				)));
			}
			let name = name.ok_or_else(|| SubmoduleError::MissingUpdateTarget {
				name: module.to_owned(),
				target: source,
			})?;
			if name == remote_head {
				return Err(SubmoduleError::Transfer(format!(
					"remote '{remote}' branch '{branch}' maps to reserved remote HEAD ref '{remote_head}'"
				)));
			}
			let oid = repository.refs().resolve_symbolic_exact(&name).await?;
			(name, oid)
		}
	};
	oid.ok_or_else(|| SubmoduleError::MissingUpdateTarget {
		name: module.to_owned(),
		target: name,
	})
}

fn typed_update_oid<H: HashAlgorithm>(
	oid: &SubmoduleObjectId,
) -> Result<ObjectId<H>, SubmoduleError> {
	oid.to_typed::<H>().ok_or_else(|| {
		SubmoduleError::Transfer(
			"submodule transfer returned a target for the wrong hash algorithm".to_owned(),
		)
	})
}

fn local_source_identity<H: HashAlgorithm>(remote: &str, target: ObjectId<H>) -> String {
	format!("local-state:{remote}:{}", target.to_hex())
}

fn durability_roots<H: HashAlgorithm>(
	recorded: ObjectId<H>,
	fetched: Vec<SubmoduleObjectId>,
) -> Result<Vec<ObjectId<H>>, SubmoduleError> {
	let mut roots = Vec::with_capacity(fetched.len() + 1);
	roots.push(recorded);
	for root in fetched {
		let root = root.to_typed::<H>().ok_or_else(|| {
			SubmoduleError::Transfer(
				"submodule transfer returned a fetched root for the wrong hash algorithm".to_owned(),
			)
		})?;
		if !roots.contains(&root) {
			roots.push(root);
		}
	}
	Ok(roots)
}

fn outcome<H: HashAlgorithm>(
	entry: &Planned<H>,
	state: UpdateOutcomeState,
	target: Option<ObjectId<H>>,
	merge: Option<UpdateMergeOutcome>,
) -> UpdateOutcome {
	UpdateOutcome {
		name: entry.declaration.name.clone(),
		path: entry.declaration.path.clone(),
		recorded: SubmoduleObjectId::from_typed(entry.recorded),
		target: target.map(SubmoduleObjectId::from_typed),
		state,
		merge,
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
	target: ObjectId<H>,
	source: &str,
	source_context: IntentSourceContext,
	remote: Option<&str>,
) -> StageIntent {
	StageIntent {
		version: 5,
		name: entry.declaration.name.clone(),
		path: entry.declaration.path.clone(),
		recorded: String::new(),
		gitlink: Some(entry.recorded.to_hex()),
		target: Some(target.to_hex()),
		remote: remote.map(str::to_owned),
		source_fingerprint: source_fingerprint(source),
		source_context: Some(source_context),
		record_remote_head: entry.record_remote_head,
	}
}

fn intent_matches_reprepare(previous: &StageIntent, current: &StageIntent) -> bool {
	intent_source_context(previous) == Some(IntentSourceContext::Superproject)
		&& current.version == 5
		&& current.source_context == Some(IntentSourceContext::Superproject)
		&& previous.name == current.name
		&& previous.path == current.path
		&& intent_gitlink(previous) == intent_gitlink(current)
		&& intent_target(previous) == intent_target(current)
		&& previous.remote == current.remote
		&& previous.record_remote_head == current.record_remote_head
		&& previous.source_fingerprint == current.source_fingerprint
}

fn intent_source_context(intent: &StageIntent) -> Option<IntentSourceContext> {
	match intent.version {
		1..=3 if intent.source_context.is_none() => Some(IntentSourceContext::Superproject),
		4 => match intent.source_context {
			Some(IntentSourceContext::Superproject | IntentSourceContext::Module) => {
				intent.source_context
			}
			_ => None,
		},
		5 => intent.source_context,
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
		4 | 5 => intent.source_context == Some(source_context) && resolved == intent.source_fingerprint,
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

fn intent_gitlink(intent: &StageIntent) -> Option<&str> {
	match intent.version {
		1..=4 if !intent.recorded.is_empty() => Some(&intent.recorded),
		5 => intent.gitlink.as_deref(),
		_ => None,
	}
}

fn intent_target(intent: &StageIntent) -> Option<&str> {
	match intent.version {
		1..=4 if !intent.recorded.is_empty() => Some(&intent.recorded),
		5 => intent.target.as_deref(),
		_ => None,
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

/// Return whether a repository owns an update control directory, rejecting malformed namespaces.
pub fn repository_has_pending_update(git: &Dir, git_dir: &Path) -> Result<bool, SubmoduleError> {
	match git.symlink_metadata(CONTROL_DIR) {
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
		Ok(_) => Err(SubmoduleError::RecoveryRequired(format!(
			"submodule update control path in '{}' is not a directory",
			git_dir.display()
		))),
		Err(source) => Err(SubmoduleError::Io {
			path: git_dir.join(CONTROL_DIR),
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
	#[cfg(unix)]
	use super::try_acquire_submodule_config_mutation_lease;
	use super::{
		CONTROL_DIR, DirectoryNamespace, INTENT_NAME, IntentSourceContext, MarkerSnapshot, Planned,
		SHARED_CONFIG_LOCK, StageIntent, UPDATE_LOCK, acquire_submodule_config_mutation_lease,
		acquire_submodule_config_setup_lease, acquire_update_lock_with_common,
		await_retained_merge_task, ensure_directory_components, finish_merge_after_validation,
		intent_gitlink, intent_matches_reprepare, intent_matches_source, intent_source_context,
		intent_target, legacy_source_fingerprint, marker_identity, module_origin_url,
		module_repository_with_worker_keepalive, parse_update_strategy, publish_new_mount_marker,
		publish_stage_intent, recommended_clone_depth, remove_staged_repository,
		rename_directory_noreplace, source_fingerprint, stage_intent,
		sync_repository_publication_parents, update_effective_config, validate_ref_fragment,
	};
	use crate::{
		ConfigViews, ConfigurationProvider, InitConfigResult, InitConfigUpdate, MarkerTargetResolver,
		SubmoduleContext, SubmoduleDeclaration, SubmoduleError, UpdateMergeOutcome, UpdateMergeResult,
		UpdateReport, UpdateRequest, UpdateStrategy,
	};
	use cap_std::{ambient_authority, fs::Dir};
	use gitana_config::GitConfig;
	use gitana_object::{HashKind, ObjectId, Sha256};
	use gitana_repository_layout::RepositoryLayout;
	use std::path::{Path, PathBuf};
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};

	fn intent(version: u32, source_fingerprint: String) -> StageIntent {
		StageIntent {
			version,
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			recorded: if version < 5 {
				"00".to_owned()
			} else {
				String::new()
			},
			gitlink: (version == 5).then(|| "00".to_owned()),
			target: (version == 5).then(|| "00".to_owned()),
			remote: None,
			source_fingerprint,
			source_context: matches!(version, 4 | 5).then_some(IntentSourceContext::Superproject),
			record_remote_head: false,
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
	fn remote_branch_components_must_not_start_with_a_dot() {
		for branch in [".topic", "foo/.topic"] {
			assert!(validate_ref_fragment(branch, "submodule branch").is_err());
		}
		for branch in ["topic.v1", "foo/topic.bar"] {
			assert!(validate_ref_fragment(branch, "submodule branch").is_ok());
		}
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
	fn legacy_recovery_reprepare_matches_semantic_identity_and_upgrades_to_v5() {
		let fingerprint = source_fingerprint("https://host/repository");
		let legacy = intent(1, fingerprint.clone());
		let current = intent(5, fingerprint);
		assert!(intent_matches_reprepare(&legacy, &current));

		let mut wrong_path = legacy.clone();
		wrong_path.path = "modules/two".to_owned();
		assert!(!intent_matches_reprepare(&wrong_path, &current));
		let mut module = current.clone();
		module.source_context = Some(IntentSourceContext::Module);
		assert!(!intent_matches_reprepare(&module, &current));
		assert!(!intent_matches_reprepare(
			&intent(6, current.source_fingerprint.clone()),
			&current
		));
	}

	#[test]
	fn v5_recovery_keeps_gitlink_and_selected_target_distinct() {
		let mut current = intent(5, source_fingerprint("https://host/repository"));
		current.gitlink = Some("11".repeat(32));
		current.target = Some("22".repeat(32));
		assert_eq!(intent_gitlink(&current), current.gitlink.as_deref());
		assert_eq!(intent_target(&current), current.target.as_deref());

		let mut changed = current.clone();
		changed.target = Some("33".repeat(32));
		assert!(!intent_matches_reprepare(&current, &changed));
		let legacy = intent(4, current.source_fingerprint.clone());
		assert_eq!(intent_gitlink(&legacy), intent_target(&legacy));
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

		let mut local = intent(4, source_fingerprint(endpoint));
		local.source_context = Some(IntentSourceContext::ModuleLocal);
		assert_eq!(intent_source_context(&local), None);
		local.version = 5;
		assert_eq!(
			intent_source_context(&local),
			Some(IntentSourceContext::ModuleLocal)
		);
	}

	#[test]
	fn stage_intent_does_not_infer_context_from_the_source_spelling() {
		let (_temporary, _context, entry) = mount_fixture("explicit-source-context");
		let remote = stage_intent(
			&entry,
			entry.recorded,
			"local-state:origin:attacker-controlled",
			IntentSourceContext::Module,
			Some("origin"),
		);
		assert_eq!(remote.source_context, Some(IntentSourceContext::Module));
		let local = stage_intent(
			&entry,
			entry.recorded,
			"ordinary-looking-source",
			IntentSourceContext::ModuleLocal,
			Some("origin"),
		);
		assert_eq!(local.source_context, Some(IntentSourceContext::ModuleLocal));
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
		let configuration = MarkerRacingConfiguration::default();
		let (temporary, context, entry) = mount_fixture("content");
		let before = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		std::fs::write(
			temporary.path().join("work/modules/one/file"),
			b"concurrent",
		)
		.unwrap();
		assert!(matches!(
			context
				.revalidate_module_mount(&entry, &before, &configuration)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("marker");
		let before = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		std::fs::write(
			temporary.path().join("work/modules/one/.git"),
			entry.pointers.marker.as_bytes(),
		)
		.unwrap();
		assert!(matches!(
			context
				.revalidate_module_mount(&entry, &before, &configuration)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("identity");
		let before = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		std::fs::rename(
			temporary.path().join("work/modules/one"),
			temporary.path().join("work/modules/old"),
		)
		.unwrap();
		std::fs::create_dir(temporary.path().join("work/modules/one")).unwrap();
		assert!(matches!(
			context
				.revalidate_module_mount(&entry, &before, &configuration)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));

		let (temporary, context, entry) = mount_fixture("marker-identity");
		let marker = temporary.path().join("work/modules/one/.git");
		std::fs::write(&marker, entry.pointers.marker.as_bytes()).unwrap();
		let before = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		let replacement = temporary.path().join("work/modules/one/.git.replacement");
		std::fs::write(&replacement, entry.pointers.marker.as_bytes()).unwrap();
		std::fs::remove_file(&marker).unwrap();
		std::fs::rename(replacement, &marker).unwrap();
		assert!(matches!(
			context
				.revalidate_module_mount(&entry, &before, &configuration)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
	}

	#[tokio::test]
	async fn conditional_marker_publish_preserves_a_concurrent_marker() {
		let (temporary, context, entry) = mount_fixture("conditional-marker");
		let configuration = MarkerRacingConfiguration::default();
		let before = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		let marker = temporary.path().join("work/modules/one/.git");
		std::fs::write(&marker, b"foreign marker\n").unwrap();

		assert!(matches!(
			context
				.publish_mount_marker(&entry, &before, &configuration)
				.await,
			Err(SubmoduleError::ForeignMount(path)) if path == "modules/one"
		));
		assert_eq!(std::fs::read(marker).unwrap(), b"foreign marker\n");
	}

	#[tokio::test]
	async fn plain_update_reloads_registration_after_acquiring_serialization() {
		let stale = GitConfig::parse("[submodule \"one\"]\n\turl = source\n\tactive = true\n").unwrap();
		let current = GitConfig::new();
		let configuration = MarkerRacingConfiguration {
			effective: Some(current),
			..Default::default()
		};
		let effective = update_effective_config(&configuration, &UpdateReport::default())
			.await
			.unwrap();

		assert_eq!(
			stale.get_string("submodule", Some("one"), "url"),
			Some("source")
		);
		assert_eq!(effective.get_raw("submodule", Some("one"), "url"), None);
	}

	#[derive(Default)]
	struct MarkerRacingConfiguration {
		marker: PathBuf,
		marker_bytes: Vec<u8>,
		effective: Option<GitConfig>,
	}

	impl MarkerTargetResolver for MarkerRacingConfiguration {
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
				(
					gitana_fs_native::directory_identity(&actual),
					gitana_fs_native::directory_identity(&expected_git_dir),
				),
				(Ok(actual), Ok(expected)) if actual == expected
			))
		}
	}

	impl ConfigurationProvider for MarkerRacingConfiguration {
		type ModuleWorktreeEdit = (Vec<u8>, Vec<u8>);

		async fn apply_init(
			&self,
			_updates: &[InitConfigUpdate],
			_active_pathspecs: &[String],
			_lease: crate::SubmoduleMutationLease,
		) -> Result<InitConfigResult, SubmoduleError> {
			unreachable!()
		}

		async fn reload(&self) -> Result<GitConfig, SubmoduleError> {
			self
				.effective
				.clone()
				.ok_or_else(|| SubmoduleError::Configuration("unexpected config reload".to_owned()))
		}

		async fn load_module_config(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
		) -> Result<GitConfig, SubmoduleError> {
			unreachable!()
		}

		async fn validate_module_config_inputs_outside_worktrees(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_selected_worktrees: Vec<Dir>,
		) -> Result<(), SubmoduleError> {
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
		) -> Result<Option<Vec<u8>>, SubmoduleError> {
			unreachable!()
		}

		async fn load_module_excludes_at(
			&self,
			_config: &GitConfig,
			_worktree: Dir,
			_worktree_root: &Path,
		) -> Result<Option<Vec<u8>>, SubmoduleError> {
			unreachable!()
		}

		async fn set_module_worktree(
			&self,
			git_dir: Dir,
			_display_path: &Path,
			_worktree: &str,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<Self::ModuleWorktreeEdit, SubmoduleError> {
			let before = git_dir.read("config").unwrap();
			let after = b"[core]\n\tworktree = ../../modules/one\n".to_vec();
			git_dir.write("config", &after).unwrap();
			let mut replacement = self.marker.as_os_str().to_owned();
			replacement.push(".replacement");
			let replacement = PathBuf::from(replacement);
			std::fs::write(&replacement, &self.marker_bytes).unwrap();
			let _ = std::fs::remove_file(&self.marker);
			std::fs::rename(replacement, &self.marker).unwrap();
			Ok((before, after))
		}

		async fn rollback_module_worktree(
			&self,
			git_dir: Dir,
			_display_path: &Path,
			edit: Self::ModuleWorktreeEdit,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<(), SubmoduleError> {
			if git_dir.read("config").unwrap() != edit.1 {
				return Err(SubmoduleError::Configuration(
					"module config changed before rollback".to_owned(),
				));
			}
			git_dir.write("config", &edit.0).unwrap();
			Ok(())
		}

		async fn plan_module_deinit(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_expected_worktree: &str,
			_mounted_worktree: Option<Dir>,
		) -> Result<crate::DeinitConfigTransition, SubmoduleError> {
			unreachable!()
		}

		async fn validate_module_deinit_target_outside_worktree(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_transition: &crate::DeinitConfigTransition,
			_publication: Option<&crate::DeinitConfigPublication>,
			_worktree: Dir,
			_worktree_root: &Path,
		) -> Result<(), SubmoduleError> {
			unreachable!()
		}

		async fn reserve_module_deinit(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_expected_worktree: &str,
			_transition: &crate::DeinitConfigTransition,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<Option<crate::DeinitConfigPublication>, SubmoduleError> {
			unreachable!()
		}

		async fn prepare_module_deinit(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_expected_worktree: &str,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<(), SubmoduleError> {
			unreachable!()
		}

		async fn restore_module_deinit_before_image(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
		}

		async fn module_deinit_before_image_requires_restore(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
		}

		async fn apply_module_deinit(
			&self,
			_git_dir: Dir,
			_display_path: &Path,
			_expected_worktree: &str,
			_transition: &crate::DeinitConfigTransition,
			_publication: Option<&crate::DeinitConfigPublication>,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
		}

		async fn plan_superproject_deinit(
			&self,
			_name: &str,
			_mounted_worktree: Option<Dir>,
			_worktree_root: &Path,
		) -> Result<crate::DeinitConfigTransition, SubmoduleError> {
			unreachable!()
		}

		async fn reserve_superproject_deinit(
			&self,
			_transition: &crate::DeinitConfigTransition,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<Option<crate::DeinitConfigPublication>, SubmoduleError> {
			unreachable!()
		}

		async fn prepare_superproject_deinit(
			&self,
			_name: &str,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<(), SubmoduleError> {
			unreachable!()
		}

		async fn restore_superproject_deinit_before_image(
			&self,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
		}

		async fn superproject_deinit_before_image_requires_restore(
			&self,
			_transition: &crate::DeinitConfigTransition,
			_publication: &crate::DeinitConfigPublication,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
		}

		async fn apply_superproject_deinit(
			&self,
			_name: &str,
			_transition: &crate::DeinitConfigTransition,
			_publication: Option<&crate::DeinitConfigPublication>,
			_lease: crate::SubmoduleMutationLease,
		) -> Result<bool, SubmoduleError> {
			unreachable!()
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
		let marker = temporary.path().join("work/modules/one/.git");
		let configuration = MarkerRacingConfiguration {
			marker: marker.clone(),
			marker_bytes: b"foreign marker\n".to_vec(),
			effective: None,
		};
		let mount_plan = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		let guard = context.acquire_update_lock().unwrap();
		let mutation_lease = guard.lease();

		assert!(matches!(
			context
				.publish_module_mount(
					&entry,
					&mount_plan,
					&module_directory,
					entry.recorded,
					true,
					(&configuration, &mutation_lease),
					None,
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
		let configuration = MarkerRacingConfiguration {
			marker: marker.clone(),
			marker_bytes: entry.pointers.marker.as_bytes().to_vec(),
			effective: None,
		};
		let mount_plan = context
			.inspect_module_mount(&entry, &configuration)
			.await
			.unwrap();
		let before_identity = match &mount_plan.marker {
			MarkerSnapshot::File { identity, .. } => *identity,
			MarkerSnapshot::Absent => panic!("fixture marker must exist"),
		};
		let guard = context.acquire_update_lock().unwrap();
		let mutation_lease = guard.lease();

		assert!(matches!(
			context
				.publish_module_mount(
					&entry,
					&mount_plan,
					&module_directory,
					entry.recorded,
					false,
					(&configuration, &mutation_lease),
					None,
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

	#[cfg(unix)]
	#[test]
	fn merge_completion_rejects_a_replaced_module_config_guard() {
		let (_temporary, context, _entry) = mount_fixture("replaced-merge-config-guard");
		let guard = context.acquire_config_update_lock().unwrap();
		let lease = guard.lease();

		context
			.git
			.rename("refs", &context.git, "detached-refs")
			.unwrap();
		context.git.create_dir("refs").unwrap();

		for result in [
			UpdateMergeResult::Completed(UpdateMergeOutcome::AlreadyUpToDate),
			UpdateMergeResult::Conflict {
				paths: vec![gitana_path::GitPath::from_utf8("conflicted").unwrap()],
			},
		] {
			assert!(matches!(
				finish_merge_after_validation(Ok(result), || lease.validate()),
				Err(SubmoduleError::RecoveryRequired(message))
					if message.contains("shared config guard changed")
			));
		}
	}

	#[test]
	fn merge_completion_validates_authority_before_propagating_an_executor_error() {
		assert!(matches!(
			finish_merge_after_validation(
				Err(SubmoduleError::Merge("executor failed".to_owned())),
				|| Err(SubmoduleError::RecoveryRequired("authority changed".to_owned())),
			),
			Err(SubmoduleError::RecoveryRequired(message)) if message == "authority changed"
		));
		assert!(matches!(
			finish_merge_after_validation(
				Err(SubmoduleError::Merge("executor failed".to_owned())),
				|| Ok(()),
			),
			Err(SubmoduleError::Merge(message)) if message == "executor failed"
		));
	}

	#[tokio::test]
	async fn cancelled_merge_waiter_does_not_cancel_the_retained_worker() {
		let retained = Arc::new(());
		let retained_weak = Arc::downgrade(&retained);
		let started = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let completed = Arc::new(AtomicBool::new(false));
		let task_started = started.clone();
		let task_release = release.clone();
		let task_completed = completed.clone();
		let merge_task = tokio::spawn(async move {
			let _retained = retained;
			task_started.store(true, Ordering::Release);
			while !task_release.load(Ordering::Acquire) {
				tokio::task::yield_now().await;
			}
			task_completed.store(true, Ordering::Release);
			Ok(UpdateMergeResult::Completed(
				UpdateMergeOutcome::AlreadyUpToDate,
			))
		});
		let waiter = tokio::spawn(await_retained_merge_task(merge_task));

		while !started.load(Ordering::Acquire) {
			tokio::task::yield_now().await;
		}
		waiter.abort();
		let _ = waiter.await;
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(retained_weak.upgrade().is_some());
		assert!(!completed.load(Ordering::Acquire));

		release.store(true, Ordering::Release);
		for _ in 0..100 {
			if completed.load(Ordering::Acquire) && retained_weak.upgrade().is_none() {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(completed.load(Ordering::Acquire));
		assert!(retained_weak.upgrade().is_none());
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn merge_repository_backend_retains_the_combined_mutation_lease() {
		let (_temporary, context, _entry) = mount_fixture("merge-worker-keepalive");
		let guard = context.acquire_config_update_lock().unwrap();
		let lease = guard.lease();
		let repository = module_repository_with_worker_keepalive::<Sha256>(
			&context.git,
			&context.layout.git_dir,
			&GitConfig::new(),
			lease.clone(),
		)
		.unwrap();
		drop(lease);
		drop(guard);

		assert!(matches!(
			context.acquire_config_update_lock(),
			Err(SubmoduleError::UpdateLocked)
		));

		drop(repository);
		let replacement = context.acquire_config_update_lock().unwrap();
		replacement.validate().unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn pending_recovery_query_is_read_under_the_retained_update_guard() {
		let (_temporary, context, _entry) = mount_fixture("retained-recovery-lock");
		let second = reopen_context(&context);
		context.git.create_dir(CONTROL_DIR).unwrap();
		let control = context.git.open_dir(CONTROL_DIR).unwrap();
		control
			.write(
				INTENT_NAME,
				serde_json::to_vec(&intent(4, source_fingerprint("source"))).unwrap(),
			)
			.unwrap();

		let lock = context.acquire_update_lock().unwrap();
		let query = context
			.pending_update_query_locked(&lock)
			.unwrap()
			.expect("the durable owner is selected");
		assert_eq!(query.pathspecs, vec![":(top,literal)modules/one"]);
		assert!(matches!(
			second.acquire_update_lock(),
			Err(SubmoduleError::UpdateLocked)
		));
		lock.validate().unwrap();
	}

	#[test]
	fn pending_recovery_query_rejects_an_empty_recorded_path() {
		let (_temporary, context, _entry) = mount_fixture("empty-recovery-path");
		context.git.create_dir(CONTROL_DIR).unwrap();
		let control = context.git.open_dir(CONTROL_DIR).unwrap();
		let mut empty = intent(4, source_fingerprint("source"));
		empty.path.clear();
		control
			.write(INTENT_NAME, serde_json::to_vec(&empty).unwrap())
			.unwrap();

		let lock = context.acquire_update_lock().unwrap();
		assert!(matches!(
			context.pending_update_query_locked(&lock),
			Err(SubmoduleError::RecoveryRequired(message))
				if message.contains("staging intent has an empty path")
		));
		assert!(control.symlink_metadata(INTENT_NAME).is_ok());
		lock.validate().unwrap();
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn update_lock_lease_retains_serialization_after_the_guard_is_dropped() {
		let (_temporary, context, _entry) = mount_fixture("leased-update-lock");
		let guard = context.acquire_update_lock().unwrap();
		let lease = guard.lease();
		drop(guard);

		assert!(matches!(
			context.acquire_update_lock(),
			Err(SubmoduleError::UpdateLocked)
		));

		drop(lease);
		let replacement = context.acquire_update_lock().unwrap();
		replacement.validate().unwrap();
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn shared_config_lock_serializes_linked_worktrees_and_survives_guard_drop() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let first_path = common_path.join("worktrees/first");
		let second_path = common_path.join("worktrees/second");
		std::fs::create_dir_all(common_path.join("refs")).unwrap();
		std::fs::create_dir_all(&first_path).unwrap();
		std::fs::create_dir_all(&second_path).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let first = Dir::open_ambient_dir(&first_path, ambient_authority()).unwrap();
		let second = Dir::open_ambient_dir(&second_path, ambient_authority()).unwrap();

		let guard =
			acquire_update_lock_with_common(&first, &first_path, &common, &common_path).unwrap();
		let lease = guard.lease();
		drop(guard);
		assert!(matches!(
			acquire_update_lock_with_common(&second, &second_path, &common, &common_path),
			Err(SubmoduleError::UpdateLocked)
		));
		drop(lease);

		let replacement =
			acquire_update_lock_with_common(&second, &second_path, &common, &common_path).unwrap();
		replacement.validate().unwrap();
		assert!(
			common
				.symlink_metadata(SHARED_CONFIG_LOCK)
				.unwrap()
				.is_file()
		);
	}

	#[cfg(unix)]
	#[test]
	fn shared_config_lease_rejects_a_replaced_visible_refs_guard() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		std::fs::create_dir_all(common_path.join("refs")).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let lease = acquire_submodule_config_mutation_lease(&common, &common_path).unwrap();

		std::fs::rename(common_path.join("refs"), common_path.join("refs-retained")).unwrap();
		std::fs::create_dir(common_path.join("refs")).unwrap();

		assert!(matches!(
			lease.validate(),
			Err(SubmoduleError::RecoveryRequired(message))
				if message.contains("shared config guard changed")
		));
	}

	#[cfg(unix)]
	#[test]
	fn shared_config_guard_rejects_a_retargeted_refs_symlink() {
		use std::os::unix::fs::symlink;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::create_dir(common_path.join("original-refs")).unwrap();
		std::fs::create_dir(common_path.join("replacement-refs")).unwrap();
		symlink("original-refs", common_path.join("refs")).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let lease = acquire_submodule_config_setup_lease(&common, &common_path).unwrap();

		std::fs::remove_file(common_path.join("refs")).unwrap();
		symlink("replacement-refs", common_path.join("refs")).unwrap();

		assert!(matches!(
			lease.validate(),
			Err(SubmoduleError::RecoveryRequired(message))
				if message.contains("shared config guard changed")
		));
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn command_setup_lease_waits_for_a_linked_worktree_config_mutation() {
		use std::sync::mpsc::{RecvTimeoutError, channel};
		use std::time::Duration;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let first_path = common_path.join("worktrees/first");
		let second_path = common_path.join("worktrees/second");
		std::fs::create_dir_all(common_path.join("refs")).unwrap();
		std::fs::create_dir_all(&first_path).unwrap();
		std::fs::create_dir_all(&second_path).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let first = Dir::open_ambient_dir(&first_path, ambient_authority()).unwrap();
		let mutation =
			acquire_update_lock_with_common(&first, &first_path, &common, &common_path).unwrap();

		let (sender, receiver) = channel();
		let thread_common_path = common_path.clone();
		let waiter = std::thread::spawn(move || {
			let common = Dir::open_ambient_dir(&thread_common_path, ambient_authority()).unwrap();
			sender.send(()).unwrap();
			acquire_submodule_config_setup_lease(&common, &thread_common_path).unwrap()
		});
		receiver.recv().unwrap();
		assert!(matches!(
			receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected)
		));

		drop(mutation);
		let lease = waiter.join().unwrap();
		assert!(matches!(
			acquire_update_lock_with_common(&first, &first_path, &common, &common_path),
			Err(SubmoduleError::UpdateLocked)
		));
		drop(lease);
		acquire_update_lock_with_common(&first, &first_path, &common, &common_path).unwrap();
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn setup_leases_are_shared_and_do_not_create_the_mutation_lock() {
		use std::sync::mpsc::{RecvTimeoutError, channel};
		use std::time::Duration;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		std::fs::create_dir_all(common_path.join("refs")).unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();

		let first = acquire_submodule_config_setup_lease(&common, &common_path).unwrap();
		let second = acquire_submodule_config_setup_lease(&common, &common_path).unwrap();
		assert!(matches!(
			common.symlink_metadata(SHARED_CONFIG_LOCK),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound
		));

		let mutation_common = common.try_clone().unwrap();
		let mutation_path = common_path.clone();
		let (started_sender, started_receiver) = channel();
		let (acquired_sender, acquired_receiver) = channel();
		let mutation = std::thread::spawn(move || {
			started_sender.send(()).unwrap();
			let lease =
				acquire_submodule_config_mutation_lease(&mutation_common, &mutation_path).unwrap();
			acquired_sender.send(()).unwrap();
			lease
		});
		started_receiver.recv().unwrap();
		assert!(matches!(
			acquired_receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Timeout)
		));
		drop(first);
		assert!(matches!(
			acquired_receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Timeout)
		));
		drop(second);
		acquired_receiver
			.recv_timeout(Duration::from_secs(1))
			.expect("mutation waits until every setup lease is released");
		let mutation = mutation.join().unwrap();
		assert!(
			common
				.symlink_metadata(SHARED_CONFIG_LOCK)
				.unwrap()
				.is_file()
		);
		drop(mutation);
	}

	#[cfg(unix)]
	#[test]
	fn shared_object_store_does_not_alias_distinct_config_guards() {
		use std::os::unix::fs::symlink;
		use std::sync::mpsc::channel;
		use std::time::Duration;

		let temporary = tempfile::tempdir().unwrap();
		let shared_objects = temporary.path().join("objects");
		let first_path = temporary.path().join("first.git");
		let second_path = temporary.path().join("second.git");
		std::fs::create_dir(&shared_objects).unwrap();
		std::fs::create_dir(&first_path).unwrap();
		std::fs::create_dir(&second_path).unwrap();
		std::fs::create_dir(first_path.join("refs")).unwrap();
		std::fs::create_dir(second_path.join("refs")).unwrap();
		symlink(&shared_objects, first_path.join("objects")).unwrap();
		symlink(&shared_objects, second_path.join("objects")).unwrap();

		let first = Dir::open_ambient_dir(&first_path, ambient_authority()).unwrap();
		let second = Dir::open_ambient_dir(&second_path, ambient_authority()).unwrap();
		let first_mutation = try_acquire_submodule_config_mutation_lease(&first, &first_path).unwrap();
		let second_mutation =
			try_acquire_submodule_config_mutation_lease(&second, &second_path).unwrap();
		drop(second_mutation);

		let (acquired_sender, acquired_receiver) = channel();
		let waiter_path = second_path.clone();
		let waiter = std::thread::spawn(move || {
			let second = Dir::open_ambient_dir(&waiter_path, ambient_authority()).unwrap();
			let lease = acquire_submodule_config_setup_lease(&second, &waiter_path).unwrap();
			acquired_sender.send(()).unwrap();
			lease
		});
		let acquired = acquired_receiver.recv_timeout(Duration::from_secs(1));
		drop(first_mutation);
		drop(waiter.join().unwrap());
		assert!(
			acquired.is_ok(),
			"an unrelated repository setup lease waited on the shared object store"
		);
	}

	#[cfg(any(unix, windows))]
	#[test]
	fn setup_lock_does_not_alias_the_main_worktree_update_guard() {
		use std::sync::mpsc::channel;
		use std::time::Duration;

		let (_temporary, context, _entry) = mount_fixture("shared-main-worktree-lock");
		let update = context.acquire_update_lock().unwrap();
		let (sender, receiver) = channel();
		let common_path = context.layout.common_dir.clone();
		let waiter = std::thread::spawn(move || {
			let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
			let lease = acquire_submodule_config_setup_lease(&common, &common_path).unwrap();
			sender.send(()).unwrap();
			lease
		});

		receiver
			.recv_timeout(Duration::from_secs(1))
			.expect("setup lease must not wait for a plain update");
		assert!(matches!(
			context.acquire_update_lock(),
			Err(SubmoduleError::UpdateLocked)
		));

		drop(update);
		drop(waiter.join().unwrap());
		let config_update = context.acquire_config_update_lock().unwrap();
		config_update.validate().unwrap();
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
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
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
			shallow: None,
		};
		let pointers = context.module_pointers(&declaration).unwrap();
		let recorded = ObjectId::from_hex(&"0".repeat(64)).unwrap();
		let entry = Planned {
			declaration,
			recorded,
			target: crate::SubmoduleUpdateTarget::Gitlink(crate::SubmoduleObjectId::from_typed(recorded)),
			recovery_target: None,
			record_remote_head: false,
			superproject_branch: None,
			source_url: Some("source".to_owned()),
			state: None,
			strategy: Some(UpdateStrategy::Checkout),
			recovering: false,
			intent_identity: None,
			module_config_lease: None,
			depth: None,
			fetch: true,
			clone_depth: None,
			pointers,
		};
		(temporary, context, entry)
	}

	#[test]
	fn recommended_depth_only_changes_initial_clone_depth() {
		let mut declaration = SubmoduleDeclaration {
			name: "one".to_owned(),
			path: "modules/one".to_owned(),
			url: Some("source".to_owned()),
			branch: None,
			update: None,
			shallow: Some(true),
		};
		let mut request = UpdateRequest::default();
		assert_eq!(recommended_clone_depth(&request, &declaration), Some(1));

		request.recommend_shallow = false;
		assert_eq!(recommended_clone_depth(&request, &declaration), None);

		request.depth = Some(3);
		assert_eq!(recommended_clone_depth(&request, &declaration), Some(3));

		request.depth = None;
		request.recommend_shallow = true;
		declaration.shallow = Some(false);
		assert_eq!(recommended_clone_depth(&request, &declaration), None);
	}

	#[test]
	fn update_strategies_parse_to_typed_execution_policy() {
		assert_eq!(
			parse_update_strategy("one", "checkout").unwrap(),
			Some(UpdateStrategy::Checkout)
		);
		assert_eq!(
			parse_update_strategy("one", "merge").unwrap(),
			Some(UpdateStrategy::Merge)
		);
		assert_eq!(parse_update_strategy("one", "none").unwrap(), None);
		assert!(matches!(
			parse_update_strategy("one", "rebase"),
			Err(SubmoduleError::UnsupportedStrategy { name, strategy })
				if name == "one" && strategy == "rebase"
		));
	}
}
