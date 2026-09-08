use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, LocalFileStore, WorkDirFs, WorktreeFileStore};
use gitana_fs_native::{EntryIdentity, directory_identity};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_repository_layout::RepositoryLayout;
use gitana_worktree::{PathspecSet, WorkTree};

use crate::{
	ConfigViews, ConfigurationProvider, InitConfigResult, InitConfigUpdate, InitNotice, InitOutcome,
	InitReport, InitRequest, MarkerTargetResolver, SubmoduleDeclaration, SubmoduleError,
	SubmoduleObjectId, SubmoduleQuery, SubmoduleStatus, SubmoduleStatusState, declarations_by_path,
	resolve_relative_url, validate_name, validate_path,
};

/// An explicit, capability-scoped superproject context.
pub struct SubmoduleContext {
	pub(crate) layout: RepositoryLayout,
	pub(crate) common: Dir,
	pub(crate) git: Dir,
	pub(crate) work: Dir,
	pub(crate) configs: ConfigViews,
	pub(crate) prefix: String,
	pub(crate) hash_kind: HashKind,
}

pub(crate) struct RelativeUrlBase {
	pub(crate) url: String,
	pub(crate) missing_remote_key: Option<String>,
}

impl SubmoduleContext {
	pub fn new(
		layout: RepositoryLayout,
		common: Dir,
		git: Dir,
		work: Dir,
		configs: ConfigViews,
		prefix: String,
		hash_kind: HashKind,
	) -> Result<Self, SubmoduleError> {
		if layout.worktree_root.is_none() {
			return Err(SubmoduleError::BareRepository);
		}
		Ok(Self {
			layout,
			common,
			git,
			work,
			configs,
			prefix,
			hash_kind,
		})
	}

	pub fn layout(&self) -> &RepositoryLayout {
		&self.layout
	}

	pub fn configs(&self) -> &ConfigViews {
		&self.configs
	}

	pub async fn declarations(&self) -> Result<Vec<SubmoduleDeclaration>, SubmoduleError> {
		let work = CapWorkDir::from_dir(self.clone_dir(&self.work, self.worktree_root())?);
		let bytes = match work.read(".gitmodules") {
			Ok(bytes) => bytes,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
			Err(source) => {
				return Err(SubmoduleError::Io {
					path: self.worktree_root().join(".gitmodules"),
					source,
				});
			}
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| SubmoduleError::MissingValue(".gitmodules is not UTF-8".to_owned()))?;
		SubmoduleDeclaration::parse_all(text)
	}

	pub async fn status<C: ConfigurationProvider>(
		&self,
		query: &SubmoduleQuery,
		configuration: &C,
	) -> Result<Vec<SubmoduleStatus>, SubmoduleError> {
		match self.hash_kind {
			HashKind::Sha1 => self.status_typed::<Sha1, C>(query, configuration).await,
			HashKind::Sha256 => self.status_typed::<Sha256, C>(query, configuration).await,
		}
	}

	pub async fn init<C: ConfigurationProvider>(
		&self,
		request: &InitRequest,
		configuration: &C,
	) -> Result<InitReport, SubmoduleError> {
		let setup = self.acquire_config_setup_lease().await?;
		self.ensure_no_repository_deinit_recovery()?;
		let effective = configuration.reload().await?;
		let initialize_only_active = should_initialize_only_active(&request.query, &effective, false);
		let planned = match self.hash_kind {
			HashKind::Sha1 => {
				self
					.init_typed::<Sha1, C>(
						request,
						configuration,
						&effective,
						initialize_only_active,
						None,
					)
					.await?
			}
			HashKind::Sha256 => {
				self
					.init_typed::<Sha256, C>(
						request,
						configuration,
						&effective,
						initialize_only_active,
						None,
					)
					.await?
			}
		};
		if let Some(report) = planned {
			return Ok(report);
		}
		drop(setup);

		let lock = self.acquire_config_update_lock()?;
		lock.validate()?;
		self.ensure_no_repository_deinit_recovery()?;
		let report = self
			.init_unlocked(request, configuration, false, lock.lease())
			.await?;
		lock.validate()?;
		Ok(report)
	}

	pub(crate) async fn init_unlocked<C: ConfigurationProvider>(
		&self,
		request: &InitRequest,
		configuration: &C,
		initialize_only_active: bool,
		lease: crate::SubmoduleMutationLease,
	) -> Result<InitReport, SubmoduleError> {
		let effective = configuration.reload().await?;
		let initialize_only_active =
			should_initialize_only_active(&request.query, &effective, initialize_only_active);
		let report = match self.hash_kind {
			HashKind::Sha1 => {
				self
					.init_typed::<Sha1, C>(
						request,
						configuration,
						&effective,
						initialize_only_active,
						Some(lease),
					)
					.await?
			}
			HashKind::Sha256 => {
				self
					.init_typed::<Sha256, C>(
						request,
						configuration,
						&effective,
						initialize_only_active,
						Some(lease),
					)
					.await?
			}
		};
		Ok(report.expect("a mutation lease always completes initialization planning"))
	}

	async fn status_typed<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		query: &SubmoduleQuery,
		configuration: &C,
	) -> Result<Vec<SubmoduleStatus>, SubmoduleError> {
		let worktree = self.worktree::<H>()?;
		let index = worktree.load_index().await?;
		let selected = self.select_gitlinks(&index, query)?;

		let declarations = declarations_by_path(self.declarations().await?)?;
		let mut statuses = Vec::with_capacity(selected.len());
		for path in selected {
			let declaration = declarations
				.get(&path)
				.ok_or_else(|| SubmoduleError::MissingMapping(path.clone()))?;
			validate_name(&declaration.name)?;
			validate_path(&declaration.path)?;
			if index.conflict(&path).is_some() {
				statuses.push(SubmoduleStatus {
					name: declaration.name.clone(),
					path,
					state: SubmoduleStatusState::Conflicted,
					oid: SubmoduleObjectId::zero::<H>(),
				});
				continue;
			}
			let Some(entry) = index.entry(&path) else {
				continue;
			};
			let (state, oid) = match self.module_head::<H, C>(declaration, configuration).await? {
				Some(head) if head != entry.oid => (
					SubmoduleStatusState::Modified,
					SubmoduleObjectId::from_typed(head),
				),
				Some(_) => (
					SubmoduleStatusState::Current,
					SubmoduleObjectId::from_typed(entry.oid),
				),
				None => (
					SubmoduleStatusState::Uninitialized,
					SubmoduleObjectId::from_typed(entry.oid),
				),
			};
			statuses.push(SubmoduleStatus {
				name: declaration.name.clone(),
				path,
				state,
				oid,
			});
		}
		Ok(statuses)
	}

	async fn init_typed<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		request: &InitRequest,
		configuration: &C,
		effective: &gitana_config::GitConfig,
		initialize_only_active: bool,
		lease: Option<crate::SubmoduleMutationLease>,
	) -> Result<Option<InitReport>, SubmoduleError> {
		struct Planned {
			name: String,
			path: String,
			activate: bool,
			safe_url: Option<String>,
			credential_url: Option<String>,
			update: Option<String>,
		}

		let worktree = self.worktree::<H>()?;
		let index = worktree.load_index().await?;
		let selected = self.select_gitlinks(&index, &request.query)?;
		let declarations = declarations_by_path(self.declarations().await?)?;
		// Resolve the superproject remote only if a selected, as-yet-unregistered module actually
		// needs a relative URL expanded. An absolute URL must not be made to fail by an unrelated,
		// malformed branch remote setting.
		let mut base: Option<RelativeUrlBase> = None;
		let mut planned = Vec::with_capacity(selected.len());
		for path in selected {
			let declaration = declarations
				.get(&path)
				.ok_or_else(|| SubmoduleError::MissingMapping(path.clone()))?;
			validate_name(&declaration.name)?;
			validate_path(&declaration.path)?;
			if let Some(strategy) = declaration.update.as_deref() {
				validate_update_strategy(&declaration.name, strategy)?;
			}
			let active = is_active(effective, &declaration.name, &declaration.path)?;
			if initialize_only_active && !active {
				continue;
			}
			let activate = !active;
			let (safe_url, credential_url) =
				match effective.get_raw("submodule", Some(&declaration.name), "url") {
					Some(Some(_)) => (None, None),
					Some(None) => {
						return Err(SubmoduleError::MissingValue(format!(
							"submodule.{}.url",
							declaration.name
						)));
					}
					None => {
						let declared = declaration
							.url
							.as_ref()
							.ok_or_else(|| SubmoduleError::MissingUrl(path.clone()))?;
						let resolved = if declared.starts_with("./") || declared.starts_with("../") {
							if base.is_none() {
								base = Some(self.branch_remote_base(&worktree, effective).await?);
							}
							let base = base.as_ref().expect("relative URL base was loaded");
							resolve_relative_url(&base.url, declared).map_err(|_| {
								SubmoduleError::InvalidRelativeUrl {
									path: path.clone(),
									url: declared.clone(),
								}
							})?
						} else {
							declared.clone()
						};
						(
							Some(gitana_remote::redact_password(&resolved)),
							Some(resolved),
						)
					}
				};
			let update = match effective.get_raw("submodule", Some(&declaration.name), "update") {
				Some(Some(strategy)) => {
					validate_update_strategy(&declaration.name, strategy)?;
					None
				}
				Some(None) => {
					return Err(SubmoduleError::MissingValue(format!(
						"submodule.{}.update",
						declaration.name
					)));
				}
				None => declaration
					.update
					.as_ref()
					.filter(|strategy| !strategy.starts_with('!'))
					.cloned(),
			};
			planned.push(Planned {
				name: declaration.name.clone(),
				path,
				activate,
				safe_url,
				credential_url,
				update,
			});
		}

		let updates: Vec<InitConfigUpdate> = planned
			.iter()
			.filter(|entry| entry.activate || entry.safe_url.is_some() || entry.update.is_some())
			.map(|entry| InitConfigUpdate {
				name: entry.name.clone(),
				activate: entry.activate,
				url_if_absent: entry.safe_url.clone(),
				update_if_absent: entry.update.clone(),
			})
			.collect();
		if !updates.is_empty() && lease.is_none() {
			return Ok(None);
		}
		let applied = if updates.is_empty() {
			InitConfigResult::default()
		} else {
			configuration
				.apply_init(
					&updates,
					&[],
					lease.expect("non-empty initialization updates require a mutation lease"),
				)
				.await?
		};
		let installed: HashSet<String> = applied.registered_urls.into_iter().collect();

		Ok(Some(InitReport {
			notices: base
				.and_then(|base| base.missing_remote_key)
				.map(|missing_key| InitNotice::AuthoritativeSuperproject { missing_key })
				.into_iter()
				.collect(),
			outcomes: planned
				.into_iter()
				.map(|entry| {
					let was_installed = installed.contains(&entry.name);
					InitOutcome {
						registered_url: was_installed.then_some(entry.safe_url).flatten(),
						credential_url: was_installed.then_some(entry.credential_url).flatten(),
						name: entry.name,
						path: entry.path,
						activated: entry.activate,
					}
				})
				.collect(),
		}))
	}

	pub(crate) async fn acquire_config_setup_lease(
		&self,
	) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
		let common = self.clone_dir(&self.common, &self.layout.common_dir)?;
		let common_dir = self.layout.common_dir.clone();
		acquire_config_setup_lease_at(common, common_dir).await
	}

	pub(crate) fn select_gitlinks<H: HashAlgorithm>(
		&self,
		index: &gitana_worktree::Index<H>,
		query: &SubmoduleQuery,
	) -> Result<Vec<String>, SubmoduleError> {
		self.select_gitlinks_with_prior_path(index, query, None)
	}

	pub(crate) fn select_gitlinks_with_prior_path<H: HashAlgorithm>(
		&self,
		index: &gitana_worktree::Index<H>,
		query: &SubmoduleQuery,
		prior_path: Option<&str>,
	) -> Result<Vec<String>, SubmoduleError> {
		let specs: Vec<&str> = query.pathspecs.iter().map(String::as_str).collect();
		let set = PathspecSet::parse(&specs, &self.prefix)?;
		if let Some(prior_path) = prior_path {
			set.matches_directory(prior_path);
		}
		let mut selected = Vec::new();
		let mut seen = HashSet::new();
		for entry in &index.entries {
			let matched = if entry.mode == 0o160000 {
				set.matches_directory(&entry.path)
			} else {
				set.matches(&entry.path)
			};
			if matched && entry.mode == 0o160000 && seen.insert(entry.path.clone()) {
				selected.push(entry.path.clone());
			}
		}
		if !query.allow_unmatched
			&& !query.pathspecs.is_empty()
			&& let Some(unmatched) = set.unmatched()
		{
			return Err(SubmoduleError::PathspecNoMatch(unmatched.to_owned()));
		}
		Ok(selected)
	}

	pub(crate) async fn branch_remote_base<H: HashAlgorithm>(
		&self,
		worktree: &WorkTree<WorktreeFileStore, CapWorkDir, H>,
		effective: &gitana_config::GitConfig,
	) -> Result<RelativeUrlBase, SubmoduleError> {
		let branch = worktree
			.repository()
			.refs()
			.read_symbolic("HEAD")
			.await?
			.and_then(|head| head.strip_prefix("refs/heads/").map(str::to_owned));
		let remote = match branch
			.as_deref()
			.map(|branch| effective.get_raw("branch", Some(branch), "remote"))
		{
			Some(Some(Some(remote))) => remote,
			Some(Some(None)) => {
				return Err(SubmoduleError::MissingValue(format!(
					"branch.{}.remote",
					branch.as_deref().unwrap_or_default()
				)));
			}
			_ => "origin",
		};
		if remote == "." {
			return Ok(RelativeUrlBase {
				url: local_url_base(self.worktree_root())?,
				missing_remote_key: None,
			});
		}
		match crate::remote_url::first_fetch_url(effective, remote)? {
			Some(url) => Ok(RelativeUrlBase {
				url: url.to_owned(),
				missing_remote_key: None,
			}),
			None => Ok(RelativeUrlBase {
				url: local_url_base(self.worktree_root())?,
				missing_remote_key: Some(format!("remote.{remote}.url")),
			}),
		}
	}

	async fn module_head<H: HashAlgorithm, C: ConfigurationProvider>(
		&self,
		declaration: &SubmoduleDeclaration,
		configuration: &C,
	) -> Result<Option<gitana_object::ObjectId<H>>, SubmoduleError> {
		let mount = self.worktree_root().join(&declaration.path);
		let Some(directory) = self.existing_mount_directory_nofollow(&declaration.path)? else {
			return Ok(None);
		};
		let work = CapWorkDir::from_dir(directory);
		let Some(metadata) = work.lstat(".git").map_err(|source| SubmoduleError::Io {
			path: mount.join(".git"),
			source,
		})?
		else {
			return Ok(None);
		};
		if !metadata.kind.is_file() {
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}
		let marker = work.read(".git").map_err(|source| SubmoduleError::Io {
			path: mount.join(".git"),
			source,
		})?;
		let marker = std::str::from_utf8(&marker)
			.map_err(|_| SubmoduleError::ForeignMount(declaration.path.clone()))?;
		let target = parse_marker_target(marker)
			.ok_or_else(|| SubmoduleError::ForeignMount(declaration.path.clone()))?;
		if !self
			.marker_targets_expected(declaration, target, configuration)
			.await?
		{
			return Err(SubmoduleError::ForeignMount(declaration.path.clone()));
		}

		let relative = Path::new("modules").join(&declaration.name);
		let module_git_dir = self.layout.git_dir.join(&relative);
		let directory = self
			.open_git_subdir_nofollow(&relative)
			.map_err(|_| SubmoduleError::InvalidRepository(declaration.name.clone()))?;
		let expected_identity =
			directory_identity(&directory).map_err(|source| SubmoduleError::Io {
				path: module_git_dir.clone(),
				source,
			})?;
		let setup_directory = directory.try_clone().map_err(|source| SubmoduleError::Io {
			path: module_git_dir.clone(),
			source,
		})?;
		let module_setup =
			acquire_config_setup_lease_at(setup_directory, module_git_dir.clone()).await?;
		module_setup.validate()?;
		let directory =
			self.reopen_module_directory(declaration, &relative, &module_git_dir, expected_identity)?;
		let config_directory = directory.try_clone().map_err(|source| SubmoduleError::Io {
			path: module_git_dir.clone(),
			source,
		})?;
		if configuration
			.module_hash_kind(config_directory, &module_git_dir)
			.await?
			!= crate::object_id::kind::<H>()
		{
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
		}
		module_setup.validate()?;
		self.reopen_module_directory(declaration, &relative, &module_git_dir, expected_identity)?;
		let files = LocalFileStore::from_dir(directory);
		let repository = Repository::<_, H>::new(ObjectStore::new(files));
		let head = repository.refs().resolve_head().await?;
		module_setup.validate()?;
		self.reopen_module_directory(declaration, &relative, &module_git_dir, expected_identity)?;
		Ok(head)
	}

	pub(crate) fn reopen_module_directory(
		&self,
		declaration: &SubmoduleDeclaration,
		relative: &Path,
		display: &Path,
		expected: EntryIdentity,
	) -> Result<Dir, SubmoduleError> {
		let directory = self
			.open_git_subdir_nofollow(relative)
			.map_err(|_| SubmoduleError::InvalidRepository(declaration.name.clone()))?;
		if directory_identity(&directory).map_err(|source| SubmoduleError::Io {
			path: display.to_owned(),
			source,
		})? != expected
		{
			return Err(SubmoduleError::InvalidRepository(declaration.name.clone()));
		}
		Ok(directory)
	}

	pub(crate) async fn marker_targets_expected<R: MarkerTargetResolver>(
		&self,
		declaration: &SubmoduleDeclaration,
		target: &str,
		configuration: &R,
	) -> Result<bool, SubmoduleError> {
		let mount = self.worktree_root().join(&declaration.path);
		let Ok(expected) = self.open_git_subdir_nofollow(&Path::new("modules").join(&declaration.name))
		else {
			return Ok(false);
		};
		configuration
			.marker_target_matches(&mount, target, expected)
			.await
	}

	pub(crate) fn open_git_subdir_nofollow(&self, relative: &Path) -> std::io::Result<Dir> {
		let mut current = self.git.try_clone()?;
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

	pub(crate) fn existing_mount_directory_nofollow(
		&self,
		path: &str,
	) -> Result<Option<Dir>, SubmoduleError> {
		let mut current = self.work.try_clone().map_err(|source| SubmoduleError::Io {
			path: self.worktree_root().to_owned(),
			source,
		})?;
		let mut traversed = PathBuf::new();
		for component in Path::new(path).components() {
			let Component::Normal(component) = component else {
				return Err(SubmoduleError::UnsafePath(path.to_owned()));
			};
			traversed.push(component);
			match current.symlink_metadata(component) {
				Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
				Ok(_) => return Err(SubmoduleError::ForeignMount(path.to_owned())),
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
				Err(source) => {
					return Err(SubmoduleError::Io {
						path: self.worktree_root().join(&traversed),
						source,
					});
				}
			}
			current = current
				.open_dir_nofollow(component)
				.map_err(|source| SubmoduleError::Io {
					path: self.worktree_root().join(&traversed),
					source,
				})?;
		}
		Ok(Some(current))
	}

	pub(crate) fn worktree<H: HashAlgorithm>(
		&self,
	) -> Result<WorkTree<WorktreeFileStore, CapWorkDir, H>, SubmoduleError> {
		let mut repository = Repository::new(ObjectStore::new(self.file_store()?));
		repository.set_effective_config(self.configs.superproject.clone());
		let work = CapWorkDir::from_dir(self.clone_dir(&self.work, self.worktree_root())?);
		Ok(WorkTree::new_located(
			repository,
			work,
			self.layout.git_dir.clone(),
			self.worktree_root().to_owned(),
		))
	}

	pub(crate) fn file_store(&self) -> Result<WorktreeFileStore, SubmoduleError> {
		Ok(WorktreeFileStore::new(
			self.clone_dir(&self.common, &self.layout.common_dir)?,
			self.clone_dir(&self.git, &self.layout.git_dir)?,
		))
	}

	pub(crate) fn clone_dir(&self, directory: &Dir, path: &Path) -> Result<Dir, SubmoduleError> {
		directory
			.try_clone()
			.map_err(|source| SubmoduleError::Open {
				path: path.to_owned(),
				source,
			})
	}

	pub(crate) fn worktree_root(&self) -> &Path {
		self
			.layout
			.worktree_root
			.as_deref()
			.expect("constructor rejects a bare repository")
	}
}

async fn acquire_config_setup_lease_at(
	directory: Dir,
	display_path: PathBuf,
) -> Result<crate::SubmoduleMutationLease, SubmoduleError> {
	tokio::task::spawn_blocking(move || {
		crate::update_operation::acquire_submodule_config_setup_lease(&directory, &display_path)
	})
	.await
	.map_err(|error| {
		SubmoduleError::Configuration(format!(
			"waiting for submodule configuration setup: {error}"
		))
	})?
}

fn local_url_base(path: &Path) -> Result<String, SubmoduleError> {
	let path = path.to_str().ok_or_else(|| {
		SubmoduleError::Configuration(format!(
			"superproject worktree root is not valid UTF-8: {}",
			path.display()
		))
	})?;
	#[cfg(windows)]
	let path = path.replace('\\', "/");
	#[cfg(not(windows))]
	let path = path.to_owned();
	Ok(path)
}

/// Parse the path portion of the canonical gitfile spelling without changing any path bytes.
/// Git removes trailing line terminators, but spaces and other non-terminator characters remain
/// significant parts of the target path.
pub(crate) fn parse_marker_target(marker: &str) -> Option<&str> {
	let target = marker
		.strip_prefix("gitdir: ")?
		.trim_end_matches(['\n', '\r']);
	(!target.is_empty()).then_some(target)
}

pub(crate) fn validate_update_strategy(name: &str, strategy: &str) -> Result<(), SubmoduleError> {
	if matches!(strategy, "checkout" | "none") {
		Ok(())
	} else {
		Err(SubmoduleError::UnsupportedStrategy {
			name: name.to_owned(),
			strategy: strategy.to_owned(),
		})
	}
}

pub(crate) fn is_active(
	config: &gitana_config::GitConfig,
	name: &str,
	path: &str,
) -> Result<bool, SubmoduleError> {
	if let Some(active) = config.get_bool("submodule", Some(name), "active")? {
		return Ok(active);
	}
	let patterns = config.get_all_raw("submodule", None, "active");
	if !patterns.is_empty() {
		let mut values = Vec::with_capacity(patterns.len());
		for pattern in patterns {
			values
				.push(pattern.ok_or_else(|| SubmoduleError::MissingValue("submodule.active".to_owned()))?);
		}
		return Ok(PathspecSet::parse(&values, "")?.matches_directory(path));
	}
	match config.get_raw("submodule", Some(name), "url") {
		Some(Some(_)) => Ok(true),
		Some(None) => Err(SubmoduleError::MissingValue(format!(
			"submodule.{name}.url"
		))),
		None => Ok(false),
	}
}

pub(crate) fn should_initialize_only_active(
	query: &SubmoduleQuery,
	config: &gitana_config::GitConfig,
	forced: bool,
) -> bool {
	forced
		|| (query.pathspecs.is_empty() && !config.get_all_raw("submodule", None, "active").is_empty())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn marker_parser_removes_only_line_terminators() {
		assert_eq!(
			parse_marker_target("gitdir: ../../modules/one \r\n"),
			Some("../../modules/one ")
		);
		assert_eq!(
			parse_marker_target("gitdir: ../../modules/one\t\n"),
			Some("../../modules/one\t")
		);
		assert_eq!(parse_marker_target("gitdir: \n"), None);
		assert_eq!(parse_marker_target("gitdir:../../modules/one\n"), None);
	}

	#[test]
	fn submodule_activation_matches_gitlinks_as_directories() {
		for pattern in ["modules/one/", "modules/one/."] {
			let config =
				gitana_config::GitConfig::parse(&format!("[submodule]\n\tactive = {pattern}\n")).unwrap();
			assert!(is_active(&config, "one", "modules/one").unwrap());
			assert!(!is_active(&config, "two", "modules/two").unwrap());
		}

		let config =
			gitana_config::GitConfig::parse("[submodule]\n\tactive = :(exclude)modules/two/\n").unwrap();
		assert!(is_active(&config, "one", "modules/one").unwrap());
		assert!(!is_active(&config, "two", "modules/two").unwrap());
	}

	#[test]
	fn implicit_initialization_filters_only_when_root_activation_exists() {
		let all = SubmoduleQuery::all();
		let explicit = SubmoduleQuery::paths(vec!["modules/two".to_owned()]);
		let no_activation =
			gitana_config::GitConfig::parse("[submodule \"one\"]\n\tactive = false\n").unwrap();
		assert!(!should_initialize_only_active(&all, &no_activation, false));

		let root_activation =
			gitana_config::GitConfig::parse("[submodule]\n\tactive = modules/one\n").unwrap();
		assert!(should_initialize_only_active(&all, &root_activation, false));
		assert!(!should_initialize_only_active(
			&explicit,
			&root_activation,
			false
		));
		assert!(should_initialize_only_active(
			&explicit,
			&root_activation,
			true
		));
	}

	#[test]
	fn rejects_ambiguous_declaration_paths() {
		let declarations = SubmoduleDeclaration::parse_all(
			"[submodule \"one\"]\npath = modules/shared\nurl = one\n[submodule \"two\"]\npath = modules/shared\nurl = two\n",
		)
		.unwrap();
		assert!(matches!(
			declarations_by_path(declarations),
			Err(SubmoduleError::DuplicateMapping(path)) if path == "modules/shared"
		));
	}

	#[test]
	fn rejects_case_folded_and_nested_declaration_collisions() {
		for text in [
			"[submodule \"one\"]\npath = modules/one\nurl = one\n[submodule \"ONE\"]\npath = modules/two\nurl = two\n",
			"[submodule \"one\"]\npath = modules\nurl = one\n[submodule \"two\"]\npath = modules/two\nurl = two\n",
		] {
			let declarations = SubmoduleDeclaration::parse_all(text).unwrap();
			assert!(matches!(
				declarations_by_path(declarations),
				Err(SubmoduleError::AmbiguousDeclaration(_))
			));
		}
	}

	#[test]
	fn rejects_canonically_equivalent_declaration_collisions() {
		for text in [
			"[submodule \"caf\u{e9}\"]\npath = modules/one\nurl = one\n[submodule \"cafe\u{301}\"]\npath = modules/two\nurl = two\n",
			"[submodule \"one\"]\npath = modules/caf\u{e9}\nurl = one\n[submodule \"two\"]\npath = modules/cafe\u{301}\nurl = two\n",
			"[submodule \"one\"]\npath = modules/caf\u{e9}\nurl = one\n[submodule \"two\"]\npath = modules/cafe\u{301}/nested\nurl = two\n",
		] {
			let declarations = SubmoduleDeclaration::parse_all(text).unwrap();
			assert!(matches!(
				declarations_by_path(declarations),
				Err(SubmoduleError::AmbiguousDeclaration(_))
			));
		}
	}

	#[test]
	fn rejects_unicode_case_folded_declaration_collisions() {
		for text in [
			"[submodule \"\u{3c3}\"]\npath = modules/one\nurl = one\n[submodule \"\u{3c2}\"]\npath = modules/two\nurl = two\n",
			"[submodule \"one\"]\npath = modules/\u{3c3}\nurl = one\n[submodule \"two\"]\npath = modules/\u{3c2}\nurl = two\n",
			"[submodule \"one\"]\npath = modules/\u{3c3}\nurl = one\n[submodule \"two\"]\npath = modules/\u{3c2}/nested\nurl = two\n",
		] {
			let declarations = SubmoduleDeclaration::parse_all(text).unwrap();
			assert!(matches!(
				declarations_by_path(declarations),
				Err(SubmoduleError::AmbiguousDeclaration(_))
			));
		}
	}
}
