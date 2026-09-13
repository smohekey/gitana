use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};

use anyhow::bail;
use cap_fs_ext::DirExt as _;
use cap_std::{ambient_authority, fs::Dir};
use gitana_config::GitConfig;
use gitana_fs_native::{
	EntryIdentity, directory_identity, entry_identity, lexical_normalize, paths_equivalent,
};
use gitana_object::HashKind;
use gitana_repository::Config;
use gitana_submodule::{
	ConfigurationProvider, DeinitConfigPublication, DeinitConfigTarget, DeinitConfigTransition,
	DeinitWorktreeAttachment, InitConfigResult, InitConfigUpdate, MarkerTargetResolver,
	SetUrlConfigurationProvider, SubmoduleError, SubmoduleMutationLease,
};
use sha2::{Digest, Sha256};

use crate::git_config;

/// Native authority for rebuilding Git's complete effective configuration stack for one worktree.
pub(crate) struct WorktreeConfiguration {
	common: Dir,
	git: Dir,
	common_dir: PathBuf,
	git_dir: PathBuf,
	#[cfg(test)]
	apply_init_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
	#[cfg(test)]
	marker_target_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
	#[cfg(test)]
	module_deinit_publication_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
	#[cfg(test)]
	set_url_root_validation_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
	#[cfg(test)]
	set_url_module_publication_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
	#[cfg(test)]
	sync_declaration_validation_pause: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
}

impl WorktreeConfiguration {
	pub(crate) fn new(common: Dir, git: Dir, common_dir: &Path, git_dir: &Path) -> Self {
		Self {
			common,
			git,
			common_dir: common_dir.to_owned(),
			git_dir: git_dir.to_owned(),
			#[cfg(test)]
			apply_init_pause: None,
			#[cfg(test)]
			marker_target_pause: None,
			#[cfg(test)]
			module_deinit_publication_pause: None,
			#[cfg(test)]
			set_url_root_validation_pause: None,
			#[cfg(test)]
			set_url_module_publication_pause: None,
			#[cfg(test)]
			sync_declaration_validation_pause: None,
		}
	}

	#[cfg(test)]
	fn with_apply_init_pause(mut self, entered: Arc<AtomicBool>, release: Arc<AtomicBool>) -> Self {
		self.apply_init_pause = Some((entered, release));
		self
	}

	#[cfg(test)]
	fn with_marker_target_pause(
		mut self,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	) -> Self {
		self.marker_target_pause = Some((entered, release));
		self
	}

	#[cfg(test)]
	fn with_module_deinit_publication_pause(
		mut self,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	) -> Self {
		self.module_deinit_publication_pause = Some((entered, release));
		self
	}

	#[cfg(test)]
	fn with_set_url_root_validation_pause(
		mut self,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	) -> Self {
		self.set_url_root_validation_pause = Some((entered, release));
		self
	}

	#[cfg(test)]
	fn with_set_url_module_publication_pause(
		mut self,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	) -> Self {
		self.set_url_module_publication_pause = Some((entered, release));
		self
	}

	#[cfg(test)]
	fn with_sync_declaration_validation_pause(
		mut self,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	) -> Self {
		self.sync_declaration_validation_pause = Some((entered, release));
		self
	}

	pub(crate) async fn hash_kind(&self) -> Result<HashKind, SubmoduleError> {
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let bytes = gitana_config_native::read_file_at(common, Path::new("config"), &path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?
			.ok_or_else(|| {
				SubmoduleError::Configuration(format!("reading {}: file not found", path.display()))
			})?;
		let config = Config::parse_bytes(&bytes)?;
		match config.object_format.as_str() {
			"sha1" => Ok(HashKind::Sha1),
			"sha256" => Ok(HashKind::Sha256),
			other => Err(SubmoduleError::Configuration(format!(
				"unsupported object format '{other}'"
			))),
		}
	}
}

pub(crate) struct ModuleWorktreeEdit {
	before: String,
	after: String,
	publication: gitana_config_native::ConfigPublication,
}

impl MarkerTargetResolver for WorktreeConfiguration {
	async fn marker_target_matches(
		&self,
		mount_path: &Path,
		target: &str,
		expected_git_dir: Dir,
	) -> Result<bool, SubmoduleError> {
		let mount_path = mount_path.to_owned();
		let target = target.to_owned();
		#[cfg(test)]
		let marker_target_pause = self.marker_target_pause.clone();
		tokio::task::spawn_blocking(move || {
			let expected = directory_identity(&expected_git_dir).map_err(|error| {
				SubmoduleError::Configuration(format!("identifying retained module repository: {error}"))
			})?;
			let target = PathBuf::from(target);
			let resolved = if target.is_absolute() {
				target
			} else {
				mount_path.join(target)
			};
			let Ok(actual) = Dir::open_ambient_dir(&resolved, ambient_authority()) else {
				return Ok(false);
			};
			let Ok(actual_identity) = directory_identity(&actual) else {
				return Ok(false);
			};
			if actual_identity != expected {
				return Ok(false);
			}
			#[cfg(test)]
			if let Some((entered, release)) = marker_target_pause {
				entered.store(true, Ordering::SeqCst);
				while !release.load(Ordering::SeqCst) {
					std::thread::yield_now();
				}
			}
			let Ok(visible) = Dir::open_ambient_dir(&resolved, ambient_authority()) else {
				return Ok(false);
			};
			Ok(
				directory_identity(&visible)
					.is_ok_and(|visible_identity| visible_identity == actual_identity),
			)
		})
		.await
		.map_err(|error| {
			SubmoduleError::Configuration(format!("resolving submodule mount marker: {error}"))
		})?
	}
}

impl ConfigurationProvider for WorktreeConfiguration {
	type ModuleWorktreeEdit = ModuleWorktreeEdit;

	async fn apply_init(
		&self,
		updates: &[InitConfigUpdate],
		active_pathspecs: &[String],
		lease: SubmoduleMutationLease,
	) -> Result<InitConfigResult, SubmoduleError> {
		lease.validate()?;
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let updates = updates.to_vec();
		let active_pathspecs = active_pathspecs.to_vec();
		#[cfg(test)]
		let apply_init_pause = self.apply_init_pause.clone();
		gitana_config_native::edit_file_at_guarded(
			common,
			Path::new("config"),
			&path,
			lease,
			move |config| {
				#[cfg(test)]
				if let Some((entered, release)) = apply_init_pause {
					entered.store(true, Ordering::SeqCst);
					while !release.load(Ordering::SeqCst) {
						std::thread::yield_now();
					}
				}
				if !active_pathspecs.is_empty() {
					config.unset("submodule", None, "active");
					for pathspec in active_pathspecs {
						config.add("submodule", None, "active", Some(&pathspec));
					}
				}
				let mut registered_urls = Vec::new();
				for update in updates {
					if update.activate {
						config.set("submodule", Some(&update.name), "active", "true")?;
					}
					if let Some(url) = update.url_if_absent {
						match config.get_raw("submodule", Some(&update.name), "url") {
							None => {
								config.set("submodule", Some(&update.name), "url", &url)?;
								registered_urls.push(update.name.clone());
							}
							Some(Some(_)) => {}
							Some(None) => {
								bail!("missing value for 'submodule.{}.url'", update.name);
							}
						}
					}
					if let Some(strategy) = update.update_if_absent
						&& config
							.get_raw("submodule", Some(&update.name), "update")
							.is_none()
					{
						config.set("submodule", Some(&update.name), "update", &strategy)?;
					}
				}
				Ok(InitConfigResult { registered_urls })
			},
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn reload(&self) -> Result<GitConfig, SubmoduleError> {
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let git = self.git.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.git_dir.display()))
		})?;
		git_config::for_worktree_at(common, git, &self.common_dir, &self.git_dir)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn load_module_config(
		&self,
		git_dir: Dir,
		display_path: &Path,
	) -> Result<GitConfig, SubmoduleError> {
		// Published submodules are ordinary repositories whose common and per-worktree git
		// directories are the same path. The native loader adds system/global/includes and the
		// invocation's command-scope layer without granting ambient authority to the core engine.
		let per_worktree = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		git_config::for_worktree_at(git_dir, per_worktree, display_path, display_path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn validate_module_config_inputs_outside_worktrees(
		&self,
		git_dir: Dir,
		display_path: &Path,
		selected_worktrees: Vec<Dir>,
	) -> Result<(), SubmoduleError> {
		if selected_worktrees.is_empty() {
			return Ok(());
		}
		let per_worktree = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		git_config::for_worktree_at_excluding(
			git_dir,
			per_worktree,
			display_path,
			display_path,
			selected_worktrees,
		)
		.await
		.map(|_| ())
		.map_err(|error| {
			SubmoduleError::Configuration(format!(
				"{error:#}; move effective config inputs outside every selected checkout before deinit"
			))
		})
	}

	async fn module_hash_kind(
		&self,
		git_dir: Dir,
		display_path: &Path,
	) -> Result<HashKind, SubmoduleError> {
		let path = display_path.join("config");
		let bytes = gitana_config_native::read_file_at(git_dir, Path::new("config"), &path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?
			.ok_or_else(|| {
				SubmoduleError::Configuration(format!("reading {}: file not found", path.display()))
			})?;
		let config = Config::parse_bytes(&bytes)?;
		match config.object_format.as_str() {
			"sha1" => Ok(HashKind::Sha1),
			"sha256" => Ok(HashKind::Sha256),
			other => Err(SubmoduleError::Configuration(format!(
				"unsupported object format '{other}'"
			))),
		}
	}

	async fn load_module_excludes(
		&self,
		config: &GitConfig,
		worktree_root: &Path,
	) -> Result<Option<String>, SubmoduleError> {
		crate::excludes::resolve_excludes_file(config, worktree_root, "")
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn load_module_excludes_at(
		&self,
		config: &GitConfig,
		worktree: Dir,
		worktree_root: &Path,
	) -> Result<Option<String>, SubmoduleError> {
		crate::excludes::resolve_excludes_file_at(config, worktree, worktree_root)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn set_module_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		worktree: &str,
		lease: SubmoduleMutationLease,
	) -> Result<Self::ModuleWorktreeEdit, SubmoduleError> {
		lease.validate()?;
		let worktree = worktree.to_owned();
		let ((before, after), publication) = gitana_config_native::edit_file_at_tracked_guarded(
			git_dir,
			Path::new("config"),
			display_path,
			lease,
			move |config| {
				let before = config.render();
				config.set("core", None, "worktree", &worktree)?;
				let after = config.render();
				Ok((before, after))
			},
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
		Ok(ModuleWorktreeEdit {
			before,
			after,
			publication,
		})
	}

	async fn rollback_module_worktree(
		&self,
		_git_dir: Dir,
		display_path: &Path,
		edit: Self::ModuleWorktreeEdit,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError> {
		lease.validate()?;
		gitana_config_native::edit_file_at_if_current_guarded(
			edit.publication,
			display_path,
			lease,
			move |config| {
				if config.render() != edit.after {
					bail!("module config changed after publishing core.worktree");
				}
				*config = GitConfig::parse(&edit.before)?;
				Ok(())
			},
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn plan_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		mounted_worktree: Option<Dir>,
	) -> Result<DeinitConfigTransition, SubmoduleError> {
		let require_attachment = mounted_worktree.is_some();
		let effective_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let target_verification_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let attachment_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let (config, target) = read_local_config(git_dir, display_path).await?;
		validate_effective_module_worktree(
			effective_dir,
			display_path,
			expected_worktree,
			require_attachment,
			None,
		)
		.await?;
		let worktree_attachment = validate_module_worktree(
			&config,
			attachment_dir,
			display_path,
			expected_worktree,
			require_attachment,
			None,
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
		if require_attachment {
			let worktree_path = resolve_module_worktree_path(display_path, expected_worktree);
			ensure_module_config_target_outside_worktree(
				target_verification_dir,
				display_path,
				mounted_worktree.expect("mounted attachment has a retained checkout"),
				&worktree_path,
				&target,
				None,
			)
			.await?;
		}
		plan_deinit_transition(config, target, worktree_attachment, |config| {
			config.unset("core", None, "worktree");
			Ok(())
		})
	}

	async fn validate_module_deinit_target_outside_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		worktree: Dir,
		worktree_root: &Path,
	) -> Result<(), SubmoduleError> {
		ensure_module_config_target_outside_worktree(
			git_dir,
			display_path,
			worktree,
			worktree_root,
			&transition.target,
			publication,
		)
		.await
	}

	async fn reserve_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError> {
		lease.validate()?;
		let reservation_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		validate_effective_module_worktree(
			git_dir,
			display_path,
			expected_worktree,
			true,
			transition.worktree_attachment.as_ref(),
		)
		.await?;
		reserve_deinit_transition(reservation_dir, display_path, transition, lease).await
	}

	async fn prepare_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError> {
		lease.validate()?;
		let preparation_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		validate_effective_module_worktree(
			git_dir,
			display_path,
			expected_worktree,
			true,
			transition.worktree_attachment.as_ref(),
		)
		.await?;
		prepare_deinit_transition(
			preparation_dir,
			display_path,
			transition,
			{
				move |config| {
					config.unset("core", None, "worktree");
					Ok(())
				}
			},
			publication,
			lease,
		)
		.await
	}

	async fn restore_module_deinit_before_image(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		lease.validate()?;
		restore_prepared_deinit_before_image(git_dir, display_path, transition, publication, lease)
			.await
	}

	async fn module_deinit_before_image_requires_restore(
		&self,
		git_dir: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError> {
		deinit_before_image_requires_restore(git_dir, display_path, transition, publication).await
	}

	async fn apply_module_deinit(
		&self,
		git_dir: Dir,
		display_path: &Path,
		expected_worktree: &str,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		lease.validate()?;
		let publication_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let final_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let legacy_attachment_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		let boundary_dir = git_dir.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
		})?;
		validate_effective_module_worktree(
			git_dir,
			display_path,
			expected_worktree,
			false,
			transition.worktree_attachment.as_ref(),
		)
		.await?;
		let boundary_attachment = match transition.worktree_attachment.clone() {
			Some(attachment) => Some(attachment),
			None => {
				let (config, _) = read_local_config(legacy_attachment_dir, display_path).await?;
				validate_module_worktree(
					&config,
					publication_dir.try_clone().map_err(|error| {
						SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
					})?,
					display_path,
					expected_worktree,
					false,
					None,
				)
				.await
				.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?
			}
		};
		#[cfg(test)]
		if let Some((entered, release)) = self.module_deinit_publication_pause.clone() {
			entered.store(true, Ordering::SeqCst);
			while !release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
		}
		let display = display_path.to_owned();
		let expected = expected_worktree.to_owned();
		let applied = apply_prepared_deinit_transition(
			publication_dir,
			display_path,
			transition,
			publication,
			lease,
			move || {
				let Some(attachment) = boundary_attachment.as_ref() else {
					return Ok(());
				};
				validate_worktree_attachment_resolution(
					boundary_dir.try_clone()?,
					&display,
					&expected,
					attachment,
				)
			},
			move |_| Ok(()),
		)
		.await?;
		ensure_effective_module_worktree_absent(final_dir, display_path).await?;
		Ok(applied)
	}

	async fn plan_superproject_deinit(
		&self,
		name: &str,
		mounted_worktree: Option<Dir>,
		worktree_root: &Path,
	) -> Result<DeinitConfigTransition, SubmoduleError> {
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let (config, target) = match mounted_worktree {
			Some(worktree) => {
				let (bytes, target, contained) =
					gitana_config_native::read_file_at_identified_with_containment(
						common,
						Path::new("config"),
						&path,
						worktree,
					)
					.await
					.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
				if contained {
					return Err(SubmoduleError::Configuration(format!(
						"superproject config target is inside the selected checkout; move it outside '{}' before deinit",
						worktree_root.display(),
					)));
				}
				(parse_local_config(bytes, &path)?, deinit_target(target))
			}
			None => read_local_config(common, &path).await?,
		};
		let name = name.to_owned();
		plan_deinit_transition(config, target, None, move |config| {
			config.remove_subsection("submodule", &name);
			Ok(())
		})
	}

	async fn reserve_superproject_deinit(
		&self,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError> {
		lease.validate()?;
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		reserve_deinit_transition(common, &path, transition, lease).await
	}

	async fn prepare_superproject_deinit(
		&self,
		name: &str,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError> {
		lease.validate()?;
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let name = name.to_owned();
		prepare_deinit_transition(
			common,
			&path,
			transition,
			move |config| {
				config.remove_subsection("submodule", &name);
				Ok(())
			},
			publication,
			lease,
		)
		.await
	}

	async fn restore_superproject_deinit_before_image(
		&self,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		lease.validate()?;
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		restore_prepared_deinit_before_image(common, &path, transition, publication, lease).await
	}

	async fn superproject_deinit_before_image_requires_restore(
		&self,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError> {
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		deinit_before_image_requires_restore(common, &path, transition, publication).await
	}

	async fn apply_superproject_deinit(
		&self,
		name: &str,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		lease.validate()?;
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let _ = name;
		apply_prepared_deinit_transition(
			common,
			&path,
			transition,
			publication,
			lease,
			|| Ok(()),
			|_| Ok(()),
		)
		.await
	}
}

impl SetUrlConfigurationProvider for WorktreeConfiguration {
	async fn revalidate_set_url_superproject(
		&self,
		worktree: Dir,
		worktree_root: &Path,
	) -> Result<(), SubmoduleError> {
		#[cfg(test)]
		if let Some((entered, release)) = self.set_url_root_validation_pause.clone() {
			entered.store(true, Ordering::SeqCst);
			while !release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
		}
		let expected = crate::RepositoryLayoutIdentity {
			worktree: Some(directory_identity(&worktree).map_err(|error| {
				SubmoduleError::Configuration(format!(
					"identifying retained worktree {}: {error}",
					worktree_root.display()
				))
			})?),
			git: directory_identity(&self.git).map_err(|error| {
				SubmoduleError::Configuration(format!(
					"identifying retained Git directory {}: {error}",
					self.git_dir.display()
				))
			})?,
			common: directory_identity(&self.common).map_err(|error| {
				SubmoduleError::Configuration(format!(
					"identifying retained common Git directory {}: {error}",
					self.common_dir.display()
				))
			})?,
		};
		let layout = gitana_repository_layout::RepositoryLayout {
			worktree_root: Some(worktree_root.to_owned()),
			git_dir: self.git_dir.clone(),
			common_dir: self.common_dir.clone(),
		};
		crate::repo::revalidate_repository_layout(&layout, expected)
			.await
			.map(|_| ())
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn plan_set_url_declaration(
		&self,
		directory: Dir,
		display_path: &Path,
		path: &str,
		url: &str,
	) -> Result<(String, DeinitConfigTransition), SubmoduleError> {
		let (mut config, target) =
			read_config_at(directory, Path::new(".gitmodules"), display_path).await?;
		let before_fingerprint = config_fingerprint(&config);
		let name = gitana_submodule::set_url(&mut config, path, url)?;
		Ok((
			name,
			DeinitConfigTransition {
				before_fingerprint,
				after_fingerprint: config_fingerprint(&config),
				target,
				worktree_attachment: None,
			},
		))
	}

	async fn read_sync_declarations(
		&self,
		directory: Dir,
		display_path: &Path,
	) -> Result<(GitConfig, DeinitConfigTransition), SubmoduleError> {
		let (config, target) =
			read_config_at(directory, Path::new(".gitmodules"), display_path).await?;
		let fingerprint = config_fingerprint(&config);
		Ok((
			config,
			DeinitConfigTransition {
				before_fingerprint: fingerprint.clone(),
				after_fingerprint: fingerprint,
				target,
				worktree_attachment: None,
			},
		))
	}

	async fn validate_sync_declarations(
		&self,
		directory: Dir,
		display_path: &Path,
		transition: &DeinitConfigTransition,
	) -> Result<(), SubmoduleError> {
		#[cfg(test)]
		if let Some((entered, release)) = self.sync_declaration_validation_pause.clone() {
			entered.store(true, Ordering::SeqCst);
			while !release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
		}
		validate_current_url_transition(
			directory,
			Path::new(".gitmodules"),
			display_path,
			transition,
		)
		.await
	}

	async fn plan_set_url_value(
		&self,
		directory: Dir,
		display_path: &Path,
		effective: &GitConfig,
		section: &str,
		subsection: &str,
		url: &str,
	) -> Result<DeinitConfigTransition, SubmoduleError> {
		let (mut config, target) = read_config_at(directory, Path::new("config"), display_path).await?;
		ensure_url_owned_by_base(&config, effective, section, subsection)?;
		let before_fingerprint = config_fingerprint(&config);
		set_single_url(&mut config, section, subsection, url)?;
		Ok(DeinitConfigTransition {
			before_fingerprint,
			after_fingerprint: config_fingerprint(&config),
			target,
			worktree_attachment: None,
		})
	}

	async fn validate_set_url_effective_value(
		&self,
		directory: Dir,
		path: gitana_submodule::SetUrlConfigPath<'_>,
		effective: &GitConfig,
		key: (&str, &str),
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
	) -> Result<(), SubmoduleError> {
		let (section, subsection) = key;
		let (config, target) = read_config_at(directory, path.relative, path.display).await?;
		let expected_target = if transition.changes() {
			let publication = publication.ok_or_else(|| {
				SubmoduleError::RecoveryRequired(
					"changed set-url transition has no prepared publication".to_owned(),
				)
			})?;
			DeinitConfigTarget::new(
				transition.target.parent(),
				Some((publication.device, publication.inode)),
				transition.target.symlinks(),
			)
		} else {
			if publication.is_some() {
				return Err(SubmoduleError::RecoveryRequired(
					"unchanged set-url transition records a publication".to_owned(),
				));
			}
			transition.target.clone()
		};
		if target != expected_target || config_fingerprint(&config) != transition.after_fingerprint {
			return Err(SubmoduleError::Configuration(format!(
				"config changed after set-url was published: {}",
				path.display.display()
			)));
		}
		ensure_url_owned_by_base(&config, effective, section, subsection)?;
		if !matches!(
			config
				.get_all_raw(section, Some(subsection), "url")
				.as_slice(),
			[Some(_)]
		) {
			return Err(SubmoduleError::Configuration(format!(
				"published {section}.{subsection}.url is not a single valued assignment"
			)));
		}
		Ok(())
	}

	async fn reserve_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		lease: SubmoduleMutationLease,
	) -> Result<Option<DeinitConfigPublication>, SubmoduleError> {
		if !transition.changes() {
			validate_current_url_transition(directory, relative_path, display_path, transition).await?;
			return Ok(None);
		}
		let prepared = gitana_config_native::reserve_file_at_if_current_guarded(
			directory,
			relative_path,
			display_path,
			native_target(&transition.target),
			lease,
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
		let (device, inode) = prepared.identity().parts();
		let name = prepared.name().to_str().ok_or_else(|| {
			SubmoduleError::Configuration("reserved set-url config name is not valid UTF-8".to_owned())
		})?;
		Ok(Some(DeinitConfigPublication::new(
			name.to_owned(),
			device,
			inode,
		)))
	}

	async fn prepare_set_url_value(
		&self,
		directory: Dir,
		path: gitana_submodule::SetUrlConfigPath<'_>,
		value: gitana_submodule::SetUrlValue<'_>,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError> {
		let transition = transition.clone();
		let section = value.section.to_owned();
		let subsection = value.subsection.to_owned();
		let url = value.url.to_owned();
		gitana_config_native::prepare_reserved_file_at_guarded(
			directory,
			path.relative,
			path.display,
			native_target(&transition.target),
			native_publication(publication),
			lease,
			move |config| {
				if config_fingerprint(config) != transition.before_fingerprint {
					bail!("config changed after set-url was planned");
				}
				set_single_url(config, &section, &subsection, &url)
					.map_err(|error| anyhow::anyhow!(error.to_string()))?;
				if config_fingerprint(config) != transition.after_fingerprint {
					bail!("set-url config transition did not produce its planned fingerprint");
				}
				Ok(())
			},
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}

	async fn apply_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: Option<&DeinitConfigPublication>,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		#[cfg(test)]
		if relative_path == Path::new("config")
			&& display_path != self.common_dir.join("config")
			&& let Some((entered, release)) = self.set_url_module_publication_pause.clone()
		{
			entered.store(true, Ordering::SeqCst);
			while !release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
		}
		apply_prepared_url_transition(
			directory,
			relative_path,
			display_path,
			transition,
			publication,
			lease,
		)
		.await
	}

	async fn restore_set_url_before_image(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<bool, SubmoduleError> {
		restore_prepared_url_before_image(
			directory,
			relative_path,
			display_path,
			transition,
			publication,
			lease,
		)
		.await
	}

	async fn set_url_before_image_requires_restore(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
	) -> Result<bool, SubmoduleError> {
		url_before_image_requires_restore(
			directory,
			relative_path,
			display_path,
			transition,
			publication,
		)
		.await
	}

	async fn discard_set_url_config(
		&self,
		directory: Dir,
		relative_path: &Path,
		display_path: &Path,
		transition: &DeinitConfigTransition,
		publication: &DeinitConfigPublication,
		lease: SubmoduleMutationLease,
	) -> Result<(), SubmoduleError> {
		gitana_config_native::discard_prepared_file_at_guarded(
			directory,
			relative_path,
			display_path,
			native_target(&transition.target),
			native_publication(publication),
			lease,
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}
}

async fn read_config_at(
	directory: Dir,
	relative_path: &Path,
	display_path: &Path,
) -> Result<(GitConfig, DeinitConfigTarget), SubmoduleError> {
	let (bytes, target) =
		gitana_config_native::read_file_at_identified(directory, relative_path, display_path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	Ok((
		parse_local_config(bytes, display_path)?,
		deinit_target(target),
	))
}

fn set_single_url(
	config: &mut GitConfig,
	section: &str,
	subsection: &str,
	url: &str,
) -> Result<(), SubmoduleError> {
	if config.get_all_raw(section, Some(subsection), "url").len() > 1 {
		return Err(
			gitana_config::ConfigError::MultipleValues(format!("{section}.{subsection}.url")).into(),
		);
	}
	config.set(section, Some(subsection), "url", url)?;
	Ok(())
}

fn ensure_url_owned_by_base(
	base: &GitConfig,
	effective: &GitConfig,
	section: &str,
	subsection: &str,
) -> Result<(), SubmoduleError> {
	let key = format!("{section}.{subsection}.url");
	if base.get_all_raw(section, Some(subsection), "url")
		!= effective.get_all_raw(section, Some(subsection), "url")
	{
		return Err(SubmoduleError::Configuration(format!(
			"effective {key} is defined outside the writable base config"
		)));
	}
	Ok(())
}

async fn validate_current_url_transition(
	directory: Dir,
	relative_path: &Path,
	display_path: &Path,
	transition: &DeinitConfigTransition,
) -> Result<(), SubmoduleError> {
	let (config, target) = read_config_at(directory, relative_path, display_path).await?;
	if target != transition.target || config_fingerprint(&config) != transition.before_fingerprint {
		return Err(SubmoduleError::Configuration(format!(
			"config changed after set-url was planned: {}",
			display_path.display()
		)));
	}
	Ok(())
}

async fn apply_prepared_url_transition(
	directory: Dir,
	relative_path: &Path,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: Option<&DeinitConfigPublication>,
	lease: SubmoduleMutationLease,
) -> Result<bool, SubmoduleError> {
	if !transition.changes() {
		if publication.is_some() {
			return Err(SubmoduleError::RecoveryRequired(
				"unchanged set-url transition records a publication".to_owned(),
			));
		}
		validate_current_url_transition(directory, relative_path, display_path, transition).await?;
		return Ok(false);
	}
	let publication = publication.ok_or_else(|| {
		SubmoduleError::RecoveryRequired(
			"changed set-url transition has no prepared publication".to_owned(),
		)
	})?;
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	let outcome = gitana_config_native::publish_prepared_file_at_guarded(
		directory,
		relative_path,
		display_path,
		native_target(&transition.target),
		native_publication(publication),
		lease,
		move |image, config| {
			let expected = match image {
				gitana_config_native::PreparedConfigImage::Before => &before,
				gitana_config_native::PreparedConfigImage::After => &after,
			};
			if config_fingerprint(config) != *expected {
				bail!("journaled set-url config image does not match its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	Ok(matches!(
		outcome,
		gitana_config_native::PreparedConfigOutcome::Published
	))
}

async fn restore_prepared_url_before_image(
	directory: Dir,
	relative_path: &Path,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: &DeinitConfigPublication,
	lease: SubmoduleMutationLease,
) -> Result<bool, SubmoduleError> {
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	gitana_config_native::restore_prepared_file_before_image_at_guarded(
		directory,
		relative_path,
		display_path,
		native_target(&transition.target),
		native_publication(publication),
		lease,
		move |image, config| {
			let expected = match image {
				gitana_config_native::PreparedConfigImage::Before => &before,
				gitana_config_native::PreparedConfigImage::After => &after,
			};
			if config_fingerprint(config) != *expected {
				bail!("journaled set-url config image does not match its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn url_before_image_requires_restore(
	directory: Dir,
	relative_path: &Path,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: &DeinitConfigPublication,
) -> Result<bool, SubmoduleError> {
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	gitana_config_native::prepared_file_before_image_requires_restore_at(
		directory,
		relative_path,
		display_path,
		native_target(&transition.target),
		native_publication(publication),
		move |image, config| {
			let expected = match image {
				gitana_config_native::PreparedConfigImage::Before => &before,
				gitana_config_native::PreparedConfigImage::After => &after,
			};
			if config_fingerprint(config) != *expected {
				bail!("journaled set-url config image does not match its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn read_local_config(
	directory: Dir,
	display_path: &Path,
) -> Result<(GitConfig, DeinitConfigTarget), SubmoduleError> {
	let (bytes, target) =
		gitana_config_native::read_file_at_identified(directory, Path::new("config"), display_path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	Ok((
		parse_local_config(bytes, display_path)?,
		deinit_target(target),
	))
}

fn parse_local_config(
	bytes: Option<Vec<u8>>,
	display_path: &Path,
) -> Result<GitConfig, SubmoduleError> {
	let bytes = bytes.unwrap_or_default();
	let text = String::from_utf8(bytes).map_err(|_| {
		SubmoduleError::Configuration(format!("{} is not UTF-8", display_path.display()))
	})?;
	GitConfig::parse(&text).map_err(SubmoduleError::from)
}

fn plan_deinit_transition(
	mut config: GitConfig,
	target: DeinitConfigTarget,
	worktree_attachment: Option<DeinitWorktreeAttachment>,
	edit: impl FnOnce(&mut GitConfig) -> anyhow::Result<()>,
) -> Result<DeinitConfigTransition, SubmoduleError> {
	let before_fingerprint = config_fingerprint(&config);
	edit(&mut config).map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	Ok(DeinitConfigTransition {
		before_fingerprint,
		after_fingerprint: config_fingerprint(&config),
		target,
		worktree_attachment,
	})
}

async fn reserve_deinit_transition(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	lease: SubmoduleMutationLease,
) -> Result<Option<DeinitConfigPublication>, SubmoduleError> {
	if !transition.changes() {
		validate_current_transition(directory, display_path, transition).await?;
		return Ok(None);
	}
	let prepared = gitana_config_native::reserve_file_at_if_current_guarded(
		directory,
		Path::new("config"),
		display_path,
		native_target(&transition.target),
		lease,
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	let (device, inode) = prepared.identity().parts();
	let name = prepared.name().to_str().ok_or_else(|| {
		SubmoduleError::Configuration("reserved deinit config name is not valid UTF-8".to_owned())
	})?;
	Ok(Some(DeinitConfigPublication::new(
		name.to_owned(),
		device,
		inode,
	)))
}

async fn prepare_deinit_transition(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	edit: impl FnOnce(&mut GitConfig) -> anyhow::Result<()> + Send + 'static,
	publication: &DeinitConfigPublication,
	lease: SubmoduleMutationLease,
) -> Result<(), SubmoduleError> {
	if !transition.changes() {
		return Err(SubmoduleError::Configuration(
			"unchanged deinit config transition has a reservation".to_owned(),
		));
	}
	let prepared = native_publication(publication);
	let transition = transition.clone();
	gitana_config_native::prepare_reserved_file_at_guarded(
		directory,
		Path::new("config"),
		display_path,
		native_target(&transition.target),
		prepared,
		lease,
		move |config| {
			if config_fingerprint(config) != transition.before_fingerprint {
				bail!("config changed after deinit was planned");
			}
			edit(config)?;
			if config_fingerprint(config) != transition.after_fingerprint {
				bail!("deinit config transition did not produce its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn apply_prepared_deinit_transition(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: Option<&DeinitConfigPublication>,
	lease: SubmoduleMutationLease,
	boundary: impl Fn() -> anyhow::Result<()> + Send + 'static,
	validate_before: impl Fn(&GitConfig) -> anyhow::Result<()> + Send + 'static,
) -> Result<bool, SubmoduleError> {
	if !transition.changes() {
		if publication.is_some() {
			return Err(SubmoduleError::Configuration(
				"unchanged deinit config transition records a publication".to_owned(),
			));
		}
		validate_current_transition(directory, display_path, transition).await?;
		return Ok(false);
	}
	let publication = publication.ok_or_else(|| {
		SubmoduleError::RecoveryRequired(
			"changed deinit config transition has no prepared publication".to_owned(),
		)
	})?;
	let prepared = native_publication(publication);
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	let outcome = gitana_config_native::publish_prepared_file_at_guarded_with_boundary(
		directory,
		Path::new("config"),
		display_path,
		native_target(&transition.target),
		prepared,
		lease,
		(boundary, move |image, config| match image {
			gitana_config_native::PreparedConfigImage::Before => {
				if config_fingerprint(config) != before {
					bail!("config changed after deinit was planned");
				}
				validate_before(config)
			}
			gitana_config_native::PreparedConfigImage::After => {
				if config_fingerprint(config) != after {
					bail!("prepared deinit config does not match its planned fingerprint");
				}
				Ok(())
			}
		}),
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	Ok(matches!(
		outcome,
		gitana_config_native::PreparedConfigOutcome::Published
	))
}

async fn restore_prepared_deinit_before_image(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: &DeinitConfigPublication,
	lease: SubmoduleMutationLease,
) -> Result<bool, SubmoduleError> {
	if !transition.changes() {
		return Err(SubmoduleError::RecoveryRequired(
			"unchanged deinit config transition records a prepared publication".to_owned(),
		));
	}
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	gitana_config_native::restore_prepared_file_before_image_at_guarded(
		directory,
		Path::new("config"),
		display_path,
		native_target(&transition.target),
		native_publication(publication),
		lease,
		move |image, config| {
			let expected = match image {
				gitana_config_native::PreparedConfigImage::Before => &before,
				gitana_config_native::PreparedConfigImage::After => &after,
			};
			if config_fingerprint(config) != *expected {
				bail!("journaled deinit config image does not match its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn deinit_before_image_requires_restore(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
	publication: &DeinitConfigPublication,
) -> Result<bool, SubmoduleError> {
	if !transition.changes() {
		return Err(SubmoduleError::RecoveryRequired(
			"unchanged deinit config transition records a prepared publication".to_owned(),
		));
	}
	let before = transition.before_fingerprint.clone();
	let after = transition.after_fingerprint.clone();
	gitana_config_native::prepared_file_before_image_requires_restore_at(
		directory,
		Path::new("config"),
		display_path,
		native_target(&transition.target),
		native_publication(publication),
		move |image, config| {
			let expected = match image {
				gitana_config_native::PreparedConfigImage::Before => &before,
				gitana_config_native::PreparedConfigImage::After => &after,
			};
			if config_fingerprint(config) != *expected {
				bail!("journaled deinit config image does not match its planned fingerprint");
			}
			Ok(())
		},
	)
	.await
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn validate_current_transition(
	directory: Dir,
	display_path: &Path,
	transition: &DeinitConfigTransition,
) -> Result<(), SubmoduleError> {
	let (bytes, target) =
		gitana_config_native::read_file_at_identified(directory, Path::new("config"), display_path)
			.await
			.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	if deinit_target(target) != transition.target {
		return Err(SubmoduleError::Configuration(
			"config target changed after deinit was planned".to_owned(),
		));
	}
	let text = String::from_utf8(bytes.unwrap_or_default()).map_err(|_| {
		SubmoduleError::Configuration(format!("{} is not UTF-8", display_path.display()))
	})?;
	let config = GitConfig::parse(&text)?;
	if config_fingerprint(&config) != transition.after_fingerprint {
		return Err(SubmoduleError::Configuration(
			"config changed after deinit was planned".to_owned(),
		));
	}
	Ok(())
}

fn deinit_target(target: gitana_config_native::ConfigTargetIdentity) -> DeinitConfigTarget {
	DeinitConfigTarget::new(
		target.parent().parts(),
		target.target().map(|identity| identity.parts()),
		target
			.symlinks()
			.iter()
			.map(|identity| identity.parts())
			.collect(),
	)
}

fn native_target(target: &DeinitConfigTarget) -> gitana_config_native::ConfigTargetIdentity {
	let parent = target.parent();
	gitana_config_native::ConfigTargetIdentity::from_parts(
		gitana_fs_native::EntryIdentity::from_parts(parent.0, parent.1),
		target
			.target()
			.map(|(device, inode)| gitana_fs_native::EntryIdentity::from_parts(device, inode)),
		target
			.symlinks()
			.into_iter()
			.map(|(device, inode)| gitana_fs_native::EntryIdentity::from_parts(device, inode))
			.collect(),
	)
}

fn native_publication(
	publication: &DeinitConfigPublication,
) -> gitana_config_native::PreparedConfigPublication {
	gitana_config_native::PreparedConfigPublication::from_parts(
		publication.name.clone().into(),
		gitana_fs_native::EntryIdentity::from_parts(publication.device, publication.inode),
	)
}

async fn validate_module_worktree(
	config: &GitConfig,
	git_dir: Dir,
	module_git_dir: &Path,
	expected_worktree: &str,
	require_attachment: bool,
	planned: Option<&DeinitWorktreeAttachment>,
) -> anyhow::Result<Option<DeinitWorktreeAttachment>> {
	validate_module_worktree_value_async(
		config.get_raw("core", None, "worktree"),
		git_dir,
		module_git_dir,
		expected_worktree,
		require_attachment,
		planned,
	)
	.await
}

async fn validate_module_worktree_value_async(
	current: Option<Option<&str>>,
	git_dir: Dir,
	module_git_dir: &Path,
	expected_worktree: &str,
	require_attachment: bool,
	planned: Option<&DeinitWorktreeAttachment>,
) -> anyhow::Result<Option<DeinitWorktreeAttachment>> {
	let current = current.map(|value| value.map(str::to_owned));
	let module_git_dir = module_git_dir.to_owned();
	let expected_worktree = expected_worktree.to_owned();
	let planned = planned.cloned();
	tokio::task::spawn_blocking(move || {
		validate_module_worktree_value(
			current.as_ref().map(|value| value.as_deref()),
			git_dir,
			&module_git_dir,
			&expected_worktree,
			require_attachment,
			planned.as_ref(),
		)
	})
	.await
	.map_err(|error| anyhow::anyhow!("module worktree validation worker failed: {error}"))?
}

async fn validate_effective_module_worktree(
	git_dir: Dir,
	display_path: &Path,
	expected_worktree: &str,
	require_attachment: bool,
	planned: Option<&DeinitWorktreeAttachment>,
) -> Result<(), SubmoduleError> {
	let attachment_dir = git_dir.try_clone().map_err(|error| {
		SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
	})?;
	let per_worktree = git_dir.try_clone().map_err(|error| {
		SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
	})?;
	let config = git_config::for_worktree_at(git_dir, per_worktree, display_path, display_path)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	if config.get_worktree_raw("core", None, "worktree").is_some() {
		return Err(SubmoduleError::Configuration(
			"module worktree-local config defines core.worktree; deinit requires the attachment to come from the base config"
				.to_owned(),
		));
	}
	if config
		.get_repository_raw_after_common_unset("core", None, "worktree")
		.is_some()
	{
		return Err(SubmoduleError::Configuration(
			"module repository config would still define core.worktree after editing the base config"
				.to_owned(),
		));
	}
	validate_module_worktree_value_async(
		config.get_repository_raw("core", None, "worktree"),
		attachment_dir,
		display_path,
		expected_worktree,
		require_attachment,
		planned,
	)
	.await
	.map(|_| ())
	.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
}

async fn ensure_effective_module_worktree_absent(
	git_dir: Dir,
	display_path: &Path,
) -> Result<(), SubmoduleError> {
	let per_worktree = git_dir.try_clone().map_err(|error| {
		SubmoduleError::Configuration(format!("opening {}: {error}", display_path.display()))
	})?;
	let config = git_config::for_worktree_at(git_dir, per_worktree, display_path, display_path)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	if config.get_worktree_raw("core", None, "worktree").is_some() {
		return Err(SubmoduleError::Configuration(
			"module worktree-local config defines core.worktree; deinit requires the attachment to come from the base config"
				.to_owned(),
		));
	}
	match config.get_repository_raw("core", None, "worktree") {
		None => Ok(()),
		Some(Some(current)) => Err(SubmoduleError::Configuration(format!(
			"module core.worktree still points to '{current}' after deinit"
		))),
		Some(None) => Err(SubmoduleError::Configuration(
			"module core.worktree has no value after deinit".to_owned(),
		)),
	}
}

fn validate_module_worktree_value(
	current: Option<Option<&str>>,
	git_dir: Dir,
	module_git_dir: &Path,
	expected_worktree: &str,
	require_attachment: bool,
	planned: Option<&DeinitWorktreeAttachment>,
) -> anyhow::Result<Option<DeinitWorktreeAttachment>> {
	match current {
		None if !require_attachment => Ok(None),
		None => bail!("module core.worktree does not identify the selected submodule mount"),
		Some(Some(current)) => capture_equivalent_worktree_attachment(
			git_dir,
			module_git_dir,
			current,
			expected_worktree,
			planned,
		)
		.map_err(|error| {
			anyhow::anyhow!(
				"module core.worktree points to '{current}', not the selected submodule mount: {error:#}"
			)
		}),
		Some(None) => bail!("module core.worktree has no value"),
	}
}

async fn ensure_module_config_target_outside_worktree(
	directory: Dir,
	module_git_dir: &Path,
	worktree: Dir,
	worktree_path: &Path,
	planned_target: &DeinitConfigTarget,
	publication: Option<&DeinitConfigPublication>,
) -> Result<(), SubmoduleError> {
	let (_, current_target, contained) =
		gitana_config_native::read_file_at_identified_with_containment(
			directory,
			Path::new("config"),
			module_git_dir,
			worktree,
		)
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))?;
	let current_target = deinit_target(current_target);
	let same_resolution = current_target.parent() == planned_target.parent()
		&& current_target.symlinks() == planned_target.symlinks();
	let expected_entry = current_target.target() == planned_target.target();
	let prepared_entry = publication.is_some_and(|publication| {
		current_target.target() == Some((publication.device, publication.inode))
	});
	let displaced_entry =
		publication.is_some() && planned_target.target().is_some() && current_target.target().is_none();
	if !same_resolution || !(expected_entry || prepared_entry || displaced_entry) {
		return Err(SubmoduleError::Configuration(
			"config target changed after deinit was planned".to_owned(),
		));
	}
	if contained {
		return Err(SubmoduleError::Configuration(format!(
			"module config target is inside the selected checkout; move it outside '{}' before deinit",
			worktree_path.display(),
		)));
	}
	Ok(())
}

type WorktreeSymlinkSnapshot = (Dir, OsString, EntryIdentity, PathBuf);

fn capture_equivalent_worktree_attachment(
	git_dir: Dir,
	module_git_dir: &Path,
	current: &str,
	expected_worktree: &str,
	planned: Option<&DeinitWorktreeAttachment>,
) -> anyhow::Result<Option<DeinitWorktreeAttachment>> {
	let current_result = pin_worktree_attachment(git_dir.try_clone()?, module_git_dir, current);
	let expected_result = pin_worktree_attachment(git_dir, module_git_dir, expected_worktree);
	match (current_result, expected_result) {
		(Ok(current_attachment), Ok(expected_attachment))
			if current_attachment.target().is_some()
				&& current_attachment.target() == expected_attachment.target() =>
		{
			ensure_planned_worktree_resolution(planned, &current_attachment)?;
			Ok(Some(current_attachment))
		}
		(Ok(current_attachment), Ok(expected_attachment))
			if current_attachment.target().is_none()
				&& expected_attachment.target().is_none()
				&& module_worktree_values_lexically_equivalent(
					module_git_dir,
					current,
					expected_worktree,
				) =>
		{
			ensure_planned_worktree_resolution(planned, &current_attachment)?;
			Ok(Some(current_attachment))
		}
		(Err(current_error), Err(expected_error))
			if planned.is_none()
				&& worktree_attachment_resolution_is_missing(&current_error)
				&& worktree_attachment_resolution_is_missing(&expected_error)
				&& module_worktree_values_lexically_equivalent(
					module_git_dir,
					current,
					expected_worktree,
				) =>
		{
			Ok(None)
		}
		(Err(error), Ok(expected_attachment))
			if planned.is_none()
				&& worktree_attachment_resolution_is_missing(&error)
				&& expected_attachment.target().is_none()
				&& module_worktree_values_lexically_equivalent(
					module_git_dir,
					current,
					expected_worktree,
				) =>
		{
			Ok(None)
		}
		(Ok(current_attachment), Err(error))
			if planned.is_none()
				&& current_attachment.target().is_none()
				&& worktree_attachment_resolution_is_missing(&error)
				&& module_worktree_values_lexically_equivalent(
					module_git_dir,
					current,
					expected_worktree,
				) =>
		{
			Ok(None)
		}
		(Err(current_error), Err(expected_error)) => {
			if worktree_attachment_resolution_is_missing(&current_error) {
				Err(expected_error)
			} else {
				Err(current_error)
			}
		}
		(Err(error), _) => Err(error),
		(_, Err(error)) => Err(error),
		_ => bail!("module core.worktree and the selected mount resolve to different directories"),
	}
}

fn worktree_attachment_resolution_is_missing(error: &anyhow::Error) -> bool {
	error.chain().any(|cause| {
		cause
			.downcast_ref::<std::io::Error>()
			.is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
	})
}

fn validate_worktree_attachment_resolution(
	git_dir: Dir,
	module_git_dir: &Path,
	expected_worktree: &str,
	planned: &DeinitWorktreeAttachment,
) -> anyhow::Result<()> {
	let current = capture_equivalent_worktree_attachment(
		git_dir,
		module_git_dir,
		planned.value(),
		expected_worktree,
		Some(planned),
	)?;
	if current.is_none() {
		bail!("module core.worktree resolution cannot be pinned at publication");
	}
	Ok(())
}

fn ensure_planned_worktree_resolution(
	planned: Option<&DeinitWorktreeAttachment>,
	current: &DeinitWorktreeAttachment,
) -> anyhow::Result<()> {
	if let Some(planned) = planned
		&& (planned.value() != current.value()
			|| planned.parent() != current.parent()
			|| planned.symlinks() != current.symlinks())
	{
		bail!("module core.worktree resolution changed after deinit was planned");
	}
	Ok(())
}

fn pin_worktree_attachment(
	git_dir: Dir,
	module_git_dir: &Path,
	value: &str,
) -> anyhow::Result<DeinitWorktreeAttachment> {
	let mut symlink_hops = 0;
	let mut symlinks = Vec::new();
	let (parent, name, expected) = resolve_worktree_attachment_target(
		git_dir,
		Path::new(value),
		&mut symlink_hops,
		&mut symlinks,
	)?;
	let target = match expected {
		Some(expected) => {
			let directory = parent.open_dir_nofollow(&name).map_err(|error| {
				anyhow::anyhow!(
					"opening resolved module worktree '{}' from '{}': {error}",
					value,
					module_git_dir.display()
				)
			})?;
			if directory_identity(&directory)? != expected || entry_identity(&parent, &name)? != expected
			{
				bail!("resolved module worktree changed while it was being opened");
			}
			Some(expected)
		}
		None => match parent.symlink_metadata(&name) {
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
			Ok(_) => bail!("resolved module worktree appeared while it was being inspected"),
			Err(error) => return Err(error.into()),
		},
	};
	for (directory, name, identity, target) in &symlinks {
		let metadata = directory.symlink_metadata(name)?;
		if !metadata.file_type().is_symlink()
			|| EntryIdentity::from_metadata(&metadata) != *identity
			|| directory.read_link_contents(name)? != *target
		{
			bail!("module core.worktree symlink changed while it was being resolved");
		}
	}
	let parent = directory_identity(&parent)?.parts();
	Ok(DeinitWorktreeAttachment::new(
		value.to_owned(),
		parent,
		target.map(EntryIdentity::parts),
		symlinks
			.iter()
			.map(|(_, _, identity, _)| identity.parts())
			.collect(),
	))
}

fn resolve_worktree_attachment_target(
	directory: Dir,
	path: &Path,
	symlink_hops: &mut usize,
	symlinks: &mut Vec<WorktreeSymlinkSnapshot>,
) -> anyhow::Result<(Dir, OsString, Option<EntryIdentity>)> {
	let (mut directory, relative) = if path.is_absolute() {
		let (anchor, relative) = absolute_worktree_anchor(path)?;
		(
			Dir::open_ambient_dir(anchor, ambient_authority())?,
			relative,
		)
	} else {
		(directory, path.to_owned())
	};
	if let Some(parent) = relative.parent() {
		for component in parent.components() {
			match component {
				Component::CurDir => {}
				Component::ParentDir => {
					directory = directory.open_parent_dir(ambient_authority())?;
				}
				Component::Normal(name) => {
					let (parent, name, expected) =
						resolve_worktree_attachment_name(directory, name, symlink_hops, symlinks)?;
					let expected = expected.ok_or_else(|| {
						std::io::Error::new(
							std::io::ErrorKind::NotFound,
							"missing directory in module core.worktree path",
						)
					})?;
					let opened = parent.open_dir_nofollow(&name)?;
					if directory_identity(&opened)? != expected || entry_identity(&parent, &name)? != expected
					{
						bail!("module core.worktree directory changed while resolving");
					}
					directory = opened;
				}
				Component::Prefix(_) | Component::RootDir => {
					bail!("invalid relative module core.worktree path")
				}
			}
		}
	}
	let name = relative
		.file_name()
		.ok_or_else(|| anyhow::anyhow!("module core.worktree path has no final component"))?;
	resolve_worktree_attachment_name(directory, name, symlink_hops, symlinks)
}

fn resolve_worktree_attachment_name(
	directory: Dir,
	name: &OsStr,
	symlink_hops: &mut usize,
	symlinks: &mut Vec<WorktreeSymlinkSnapshot>,
) -> anyhow::Result<(Dir, OsString, Option<EntryIdentity>)> {
	match directory.symlink_metadata(name) {
		Ok(metadata) if metadata.file_type().is_symlink() => {
			*symlink_hops += 1;
			if *symlink_hops > 40 {
				bail!("too many symbolic links while resolving module core.worktree");
			}
			let identity = EntryIdentity::from_metadata(&metadata);
			let target = directory.read_link_contents(name)?;
			let current = directory.symlink_metadata(name)?;
			if !current.file_type().is_symlink() || EntryIdentity::from_metadata(&current) != identity {
				bail!("module core.worktree symlink changed while resolving");
			}
			symlinks.push((
				directory.try_clone()?,
				name.to_owned(),
				identity,
				target.clone(),
			));
			resolve_worktree_attachment_target(directory, &target, symlink_hops, symlinks)
		}
		Ok(metadata) => Ok((
			directory,
			name.to_owned(),
			Some(EntryIdentity::from_metadata(&metadata)),
		)),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			Ok((directory, name.to_owned(), None))
		}
		Err(error) => Err(error.into()),
	}
}

fn absolute_worktree_anchor(path: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
	let mut components = path.components();
	let mut anchor = PathBuf::new();
	match components.next() {
		Some(Component::Prefix(prefix)) => {
			anchor.push(prefix.as_os_str());
			match components.next() {
				Some(Component::RootDir) => anchor.push(std::path::MAIN_SEPARATOR_STR),
				_ => bail!("module core.worktree path is not absolute"),
			}
		}
		Some(Component::RootDir) => anchor.push(std::path::MAIN_SEPARATOR_STR),
		_ => bail!("module core.worktree path is not absolute"),
	}
	Ok((anchor, components.collect()))
}

fn module_worktree_values_lexically_equivalent(
	module_git_dir: &Path,
	left: &str,
	right: &str,
) -> bool {
	let left_value = Path::new(left);
	let right_value = Path::new(right);
	if !worktree_value_is_lexically_safe(left_value) || !worktree_value_is_lexically_safe(right_value)
	{
		return false;
	}
	let left = resolve_module_worktree_path(module_git_dir, left);
	let right = resolve_module_worktree_path(module_git_dir, right);
	paths_equivalent(&lexical_normalize(&left), &lexical_normalize(&right))
}

#[cfg(test)]
async fn module_worktrees_equivalent(module_git_dir: &Path, left: &str, right: &str) -> bool {
	let module_git_dir = module_git_dir.to_owned();
	let left = left.to_owned();
	let right = right.to_owned();
	tokio::task::spawn_blocking(move || {
		let Ok(git_dir) = Dir::open_ambient_dir(&module_git_dir, ambient_authority()) else {
			return module_worktree_values_lexically_equivalent(&module_git_dir, &left, &right);
		};
		capture_equivalent_worktree_attachment(git_dir, &module_git_dir, &left, &right, None).is_ok()
	})
	.await
	.unwrap_or(false)
}

fn resolve_module_worktree_path(module_git_dir: &Path, value: &str) -> PathBuf {
	let path = Path::new(value);
	if path.is_absolute() {
		path.to_owned()
	} else {
		module_git_dir.join(path)
	}
}

fn worktree_value_is_lexically_safe(value: &Path) -> bool {
	let mut saw_normal = false;
	for component in value.components() {
		match component {
			Component::Normal(_) => saw_normal = true,
			Component::ParentDir if saw_normal => return false,
			Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => {}
		}
	}
	true
}

fn config_fingerprint(config: &GitConfig) -> String {
	format!("{:x}", Sha256::digest(config.render().as_bytes()))
}

#[cfg(all(test, unix))]
mod tests {
	use cap_std::ambient_authority;
	use gitana_object::{ObjectId, Sha1};
	use gitana_repository_layout::RepositoryLayout;
	use gitana_submodule::{
		ConfigViews, FetchRepository, FetchSource, FetchedTransfer, InitRequest, PrepareRepository,
		PrepareSource, PreparedTransfer, RepositoryTransfer, SetUrlRequest, SubmoduleContext,
		SubmoduleMutationLease, SubmoduleObjectId, SubmoduleQuery, SyncRequest, UpdateOutcomeState,
		UpdateRequest,
	};
	use gitana_worktree::{Index, IndexEntry, Stat};
	use std::os::unix::fs::symlink;
	use std::sync::{Arc, atomic::AtomicUsize};

	use super::*;

	fn mutation_lease() -> SubmoduleMutationLease {
		SubmoduleMutationLease::retain(Arc::new(()))
	}

	#[test]
	fn set_url_ownership_rejects_matching_values_from_an_overlay() {
		let base = GitConfig::parse("[submodule \"one\"]\n\turl = next\n").unwrap();
		let mut unrelated = base.clone();
		unrelated
			.overlay([gitana_config::GitConfigSource::parse("[core]\n\tfilemode = false\n").unwrap()]);
		ensure_url_owned_by_base(&base, &unrelated, "submodule", "one").unwrap();

		let mut effective = base.clone();
		effective
			.overlay([
				gitana_config::GitConfigSource::parse("[submodule \"one\"]\n\turl = next\n").unwrap(),
			]);

		let error = ensure_url_owned_by_base(&base, &effective, "submodule", "one").unwrap_err();
		assert!(
			error
				.to_string()
				.contains("defined outside the writable base config")
		);
	}

	#[tokio::test]
	async fn set_url_revalidates_the_root_after_acquiring_its_mutation_locks() {
		let temporary = tempfile::tempdir().unwrap();
		let temporary_root = temporary.path().canonicalize().unwrap();
		let worktree = temporary_root.join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		let config = "[core]\n\trepositoryformatversion = 0\n";
		let modules = "[submodule \"one\"]\n\tpath = modules/one\n\turl = old\n";
		std::fs::write(git_dir.join("config"), config).unwrap();
		std::fs::write(worktree.join(".gitmodules"), modules).unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let configuration = Arc::new(
			WorktreeConfiguration::new(
				common.try_clone().unwrap(),
				git.try_clone().unwrap(),
				&git_dir,
				&git_dir,
			)
			.with_set_url_root_validation_pause(Arc::clone(&entered), Arc::clone(&release)),
		);
		let effective = configuration.reload().await.unwrap();
		let context = Arc::new(
			SubmoduleContext::new(
				RepositoryLayout {
					worktree_root: Some(worktree.clone()),
					git_dir: git_dir.clone(),
					common_dir: git_dir.clone(),
				},
				common,
				git,
				Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
				ConfigViews::new(effective),
				String::new(),
				HashKind::Sha1,
			)
			.unwrap(),
		);
		let running_context = Arc::clone(&context);
		let running_configuration = Arc::clone(&configuration);
		let task = tokio::spawn(async move {
			running_context
				.set_url(
					&SetUrlRequest {
						path: "modules/one".to_owned(),
						url: "next".to_owned(),
					},
					running_configuration.as_ref(),
				)
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(entered.load(Ordering::SeqCst));

		let displaced = temporary_root.join("displaced");
		std::fs::rename(&worktree, &displaced).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(git_dir.join("config"), config).unwrap();
		std::fs::write(worktree.join(".gitmodules"), "replacement\n").unwrap();
		release.store(true, Ordering::SeqCst);

		let error = task.await.unwrap().unwrap_err();
		assert!(
			error
				.to_string()
				.contains("worktree changed while waiting for repository setup"),
			"unexpected error: {error}"
		);
		assert_eq!(
			std::fs::read_to_string(displaced.join(".gitmodules")).unwrap(),
			modules
		);
		assert_eq!(
			std::fs::read_to_string(worktree.join(".gitmodules")).unwrap(),
			"replacement\n"
		);
		assert!(!displaced.join(".git/gitana-submodule-set-url").exists());
		assert!(!worktree.join(".git/gitana-submodule-set-url").exists());
	}

	#[tokio::test]
	async fn set_url_revalidates_the_visible_module_after_config_publication() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().canonicalize().unwrap().join("work");
		let git_dir = worktree.join(".git");
		let module_git = git_dir.join("modules/one");
		let module_worktree = worktree.join("modules/one");
		for directory in [
			git_dir.join("objects"),
			git_dir.join("refs"),
			module_git.join("objects"),
			module_git.join("refs"),
			module_worktree.clone(),
		] {
			std::fs::create_dir_all(directory).unwrap();
		}
		let super_config = "[core]\n\trepositoryformatversion = 0\n\
			[submodule \"one\"]\n\tactive = true\n\turl = old\n";
		let module_config = "[core]\n\trepositoryformatversion = 0\n\tworktree = ../../../modules/one\n\
			[remote \"origin\"]\n\turl = old\n";
		let modules = "[submodule \"one\"]\n\tpath = modules/one\n\turl = old\n";
		std::fs::write(git_dir.join("config"), super_config).unwrap();
		std::fs::write(worktree.join(".gitmodules"), modules).unwrap();
		std::fs::write(module_git.join("config"), module_config).unwrap();
		std::fs::write(module_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			module_worktree.join(".git"),
			"gitdir: ../../.git/modules/one\n",
		)
		.unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let configuration = Arc::new(
			WorktreeConfiguration::new(
				common.try_clone().unwrap(),
				git.try_clone().unwrap(),
				&git_dir,
				&git_dir,
			)
			.with_set_url_module_publication_pause(Arc::clone(&entered), Arc::clone(&release)),
		);
		let effective = configuration.reload().await.unwrap();
		let context = Arc::new(
			SubmoduleContext::new(
				RepositoryLayout {
					worktree_root: Some(worktree.clone()),
					git_dir: git_dir.clone(),
					common_dir: git_dir.clone(),
				},
				common,
				git,
				Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
				ConfigViews::new(effective),
				String::new(),
				HashKind::Sha1,
			)
			.unwrap(),
		);
		let running_context = Arc::clone(&context);
		let running_configuration = Arc::clone(&configuration);
		let task = tokio::spawn(async move {
			running_context
				.set_url(
					&SetUrlRequest {
						path: "modules/one".to_owned(),
						url: "next".to_owned(),
					},
					running_configuration.as_ref(),
				)
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			if task.is_finished() {
				break;
			}
			tokio::task::yield_now().await;
		}
		if !entered.load(Ordering::SeqCst) {
			panic!(
				"set-url finished before module publication: {:?}",
				task.await.unwrap()
			);
		}

		let displaced = git_dir.join("modules/displaced");
		std::fs::rename(&module_git, &displaced).unwrap();
		std::fs::create_dir_all(module_git.join("objects")).unwrap();
		std::fs::create_dir(module_git.join("refs")).unwrap();
		let replacement_config = "[core]\n\trepositoryformatversion = 0\n\tworktree = ../../../modules/one\n\
			[remote \"origin\"]\n\turl = replacement\n";
		std::fs::write(module_git.join("config"), replacement_config).unwrap();
		std::fs::write(module_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		release.store(true, Ordering::SeqCst);

		let error = task.await.unwrap().unwrap_err();
		assert!(matches!(error, SubmoduleError::InvalidRepository(ref name) if name == "one"));
		assert_eq!(
			std::fs::read_to_string(module_git.join("config")).unwrap(),
			replacement_config
		);
		assert!(
			std::fs::read_to_string(displaced.join("config"))
				.unwrap()
				.contains("url = next")
		);
		assert!(
			git_dir
				.join("gitana-submodule-set-url/intent.json")
				.exists()
		);
		assert!(displaced.join("gitana-submodule-set-url.claim").exists());
		assert!(!module_git.join("gitana-submodule-set-url.claim").exists());
	}

	struct UnexpectedTransfer;

	impl RepositoryTransfer for UnexpectedTransfer {
		type Error = std::io::Error;
		type PreparedSource = ();

		fn resolve_source_identity(&self, _request: &PrepareSource) -> Result<String, Self::Error> {
			panic!("an unregistered module must not resolve a transfer source")
		}

		fn resolve_fetch_source_identity(&self, _source: &FetchSource) -> Result<String, Self::Error> {
			panic!("an unregistered module must not resolve a fetch source")
		}

		async fn prepare_source(
			&self,
			_request: PrepareSource,
			_lease: SubmoduleMutationLease,
		) -> Result<PreparedTransfer<Self::PreparedSource>, Self::Error> {
			panic!("an unregistered module must not prepare a transfer source")
		}

		async fn populate_prepared(
			&self,
			_source: Self::PreparedSource,
			_request: PrepareRepository,
		) -> Result<Vec<SubmoduleObjectId>, Self::Error> {
			panic!("an unregistered module must not populate a repository")
		}

		async fn fetch_target(
			&self,
			_request: FetchRepository,
			_lease: SubmoduleMutationLease,
		) -> Result<FetchedTransfer, Self::Error> {
			panic!("an unregistered module must not fetch a repository")
		}
	}

	struct SerializedConfigTransfer {
		expected: &'static str,
		observations: Arc<AtomicUsize>,
	}

	impl SerializedConfigTransfer {
		fn observe(&self, request: &PrepareSource) {
			assert_eq!(
				request.config.get_string("snapshot", None, "value"),
				Some(self.expected)
			);
			self.observations.fetch_add(1, Ordering::SeqCst);
		}
	}

	impl RepositoryTransfer for SerializedConfigTransfer {
		type Error = std::io::Error;
		type PreparedSource = ();

		fn resolve_source_identity(&self, request: &PrepareSource) -> Result<String, Self::Error> {
			self.observe(request);
			Ok(request.source_url.clone())
		}

		fn resolve_fetch_source_identity(&self, _source: &FetchSource) -> Result<String, Self::Error> {
			panic!("a missing module must not resolve an existing-repository source")
		}

		async fn prepare_source(
			&self,
			request: PrepareSource,
			_lease: SubmoduleMutationLease,
		) -> Result<PreparedTransfer<Self::PreparedSource>, Self::Error> {
			self.observe(&request);
			Err(std::io::Error::other(
				"stop after observing serialized configuration",
			))
		}

		async fn populate_prepared(
			&self,
			_source: Self::PreparedSource,
			_request: PrepareRepository,
		) -> Result<Vec<SubmoduleObjectId>, Self::Error> {
			panic!("the test transfer stops before repository population")
		}

		async fn fetch_target(
			&self,
			_request: FetchRepository,
			_lease: SubmoduleMutationLease,
		) -> Result<FetchedTransfer, Self::Error> {
			panic!("a missing module must not fetch an existing repository")
		}
	}

	#[tokio::test]
	async fn module_worktree_validation_accepts_equivalent_native_spellings() {
		let module = Path::new("/repo/.git/modules/one");
		assert!(
			module_worktrees_equivalent(module, "../../../modules/./one", "/repo/modules/one").await
		);

		let temporary = tempfile::tempdir().unwrap();
		let module = temporary.path().join(".git/modules/one");
		let mount = temporary.path().join("modules/one");
		std::fs::create_dir_all(&module).unwrap();
		std::fs::create_dir_all(&mount).unwrap();
		assert!(
			module_worktrees_equivalent(
				&module,
				"../../../modules/one/../one",
				mount.to_str().unwrap()
			)
			.await
		);
		assert!(
			!module_worktrees_equivalent(&module, "../../../modules/two", "../../../modules/one").await
		);

		std::fs::create_dir_all(temporary.path().join("foreign/child")).unwrap();
		std::fs::create_dir_all(temporary.path().join("foreign/modules/one")).unwrap();
		symlink(
			temporary.path().join("foreign/child"),
			temporary.path().join("trap"),
		)
		.unwrap();
		assert!(
			!module_worktrees_equivalent(
				&module,
				"../../../trap/../modules/one",
				"../../../modules/one"
			)
			.await
		);
	}

	#[test]
	fn module_worktree_validation_uses_lexical_fallback_only_for_missing_paths() {
		let temporary = tempfile::tempdir().unwrap();
		let module = temporary.path().join(".git/modules/one");
		std::fs::create_dir_all(&module).unwrap();
		let module_dir = Dir::open_ambient_dir(&module, ambient_authority()).unwrap();
		let missing = temporary.path().join("absent/one");

		let attachment = capture_equivalent_worktree_attachment(
			module_dir.try_clone().unwrap(),
			&module,
			"../../../absent/one",
			missing.to_str().unwrap(),
			None,
		)
		.unwrap();
		assert!(attachment.is_none());

		symlink("loop", module.join("loop")).unwrap();
		let error = capture_equivalent_worktree_attachment(module_dir, &module, "loop", "./loop", None)
			.expect_err("a symlink loop must not be accepted through lexical equivalence");
		assert!(error.to_string().contains("too many symbolic links"));
	}

	#[tokio::test]
	async fn marker_resolution_compares_the_opened_target_identity() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let mount = worktree.join("modules/one");
		let module = worktree.join(".git/modules/one");
		let foreign = temporary.path().join("foreign");
		std::fs::create_dir_all(&mount).unwrap();
		std::fs::create_dir_all(&module).unwrap();
		std::fs::create_dir_all(foreign.join("child")).unwrap();
		std::fs::create_dir_all(foreign.join(".git/modules/one")).unwrap();
		symlink(foreign.join("child"), worktree.join("trap")).unwrap();

		let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let configuration = WorktreeConfiguration::new(
			root.try_clone().unwrap(),
			root,
			temporary.path(),
			temporary.path(),
		);
		let expected = Dir::open_ambient_dir(&module, ambient_authority()).unwrap();
		assert!(
			configuration
				.marker_target_matches(
					&mount,
					"../../.git/modules/one",
					expected.try_clone().unwrap(),
				)
				.await
				.unwrap()
		);
		assert!(
			configuration
				.marker_target_matches(
					&mount,
					module.to_str().unwrap(),
					expected.try_clone().unwrap(),
				)
				.await
				.unwrap()
		);
		assert!(
			!configuration
				.marker_target_matches(&mount, "../../trap/../.git/modules/one", expected,)
				.await
				.unwrap()
		);
	}

	#[tokio::test]
	async fn marker_resolution_revalidates_the_visible_target_after_opening() {
		let temporary = tempfile::tempdir().unwrap();
		let mount = temporary.path().join("mount");
		let module = temporary.path().join("module");
		let foreign = temporary.path().join("foreign");
		std::fs::create_dir(&mount).unwrap();
		std::fs::create_dir(&module).unwrap();
		std::fs::create_dir(&foreign).unwrap();
		let alias = mount.join("alias");
		symlink(&module, &alias).unwrap();

		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let configuration = WorktreeConfiguration::new(
			root.try_clone().unwrap(),
			root,
			temporary.path(),
			temporary.path(),
		)
		.with_marker_target_pause(Arc::clone(&entered), Arc::clone(&release));
		let expected = Dir::open_ambient_dir(&module, ambient_authority()).unwrap();
		let task = tokio::spawn(async move {
			configuration
				.marker_target_matches(&mount, "alias", expected)
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(entered.load(Ordering::SeqCst));

		std::fs::remove_file(&alias).unwrap();
		symlink(&foreign, &alias).unwrap();
		release.store(true, Ordering::SeqCst);
		assert!(!task.await.unwrap().unwrap());
	}

	#[tokio::test]
	async fn initialization_reloads_registration_after_acquiring_the_mutation_lock() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			worktree.join(".gitmodules"),
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		let stale_config = "[core]\n\trepositoryformatversion = 0\n\
			[submodule \"one\"]\n\tactive = true\n\turl = https://example.invalid/one\n";
		std::fs::write(git_dir.join("config"), stale_config).unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(
			common.try_clone().unwrap(),
			git.try_clone().unwrap(),
			&git_dir,
			&git_dir,
		);
		let stale = configuration.reload().await.unwrap();
		let context = SubmoduleContext::new(
			RepositoryLayout {
				worktree_root: Some(worktree.clone()),
				git_dir: git_dir.clone(),
				common_dir: git_dir.clone(),
			},
			common,
			git,
			Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
			ConfigViews::new(stale),
			String::new(),
			HashKind::Sha1,
		)
		.unwrap();

		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		let report = context
			.init(&InitRequest::default(), &configuration)
			.await
			.unwrap();

		assert_eq!(report.outcomes.len(), 1);
		assert!(report.outcomes[0].activated);
		assert_eq!(
			report.outcomes[0].registered_url.as_deref(),
			Some("https://example.invalid/one")
		);
		let config = std::fs::read_to_string(git_dir.join("config")).unwrap();
		assert!(config.contains("active = true"));
		assert!(config.contains("url = https://example.invalid/one"));
	}

	#[tokio::test]
	async fn plain_update_reloads_registration_after_acquiring_the_update_lock() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			worktree.join(".gitmodules"),
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\
			 [submodule \"one\"]\n\tactive = true\n\turl = https://example.invalid/one\n",
		)
		.unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(
			common.try_clone().unwrap(),
			git.try_clone().unwrap(),
			&git_dir,
			&git_dir,
		);
		let stale = configuration.reload().await.unwrap();
		let context = SubmoduleContext::new(
			RepositoryLayout {
				worktree_root: Some(worktree.clone()),
				git_dir: git_dir.clone(),
				common_dir: git_dir.clone(),
			},
			common,
			git,
			Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
			ConfigViews::new(stale),
			String::new(),
			HashKind::Sha1,
		)
		.unwrap();

		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		let report = context
			.update(
				&UpdateRequest {
					query: Default::default(),
					initialize: false,
					depth: None,
					recommend_shallow: true,
					remote: false,
					fetch: true,
					strategy: None,
					initialize_only_active: false,
					reflog_committer: None,
				},
				&configuration,
				&UnexpectedTransfer,
				&(),
			)
			.await
			.unwrap();

		assert_eq!(report.outcomes.len(), 1);
		assert_eq!(
			report.outcomes[0].state,
			UpdateOutcomeState::SkippedUnregistered
		);
		assert!(!worktree.join("modules/one").exists());
		assert!(!git_dir.join("modules/one").exists());
	}

	#[tokio::test]
	async fn initial_transfer_uses_the_serialized_post_lock_config_snapshot() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			worktree.join(".gitmodules"),
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\
			 [snapshot]\n\tvalue = stale\n\
			 [submodule \"one\"]\n\tactive = true\n\turl = https://example.invalid/one\n",
		)
		.unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(
			common.try_clone().unwrap(),
			git.try_clone().unwrap(),
			&git_dir,
			&git_dir,
		);
		let stale = configuration.reload().await.unwrap();
		let context = SubmoduleContext::new(
			RepositoryLayout {
				worktree_root: Some(worktree.clone()),
				git_dir: git_dir.clone(),
				common_dir: git_dir.clone(),
			},
			common,
			git,
			Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
			ConfigViews::new(stale),
			String::new(),
			HashKind::Sha1,
		)
		.unwrap();

		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\
			 [snapshot]\n\tvalue = fresh\n\
			 [submodule \"one\"]\n\tactive = true\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		let observations = Arc::new(AtomicUsize::new(0));
		let transfer = SerializedConfigTransfer {
			expected: "fresh",
			observations: observations.clone(),
		};
		let error = context
			.update(
				&UpdateRequest {
					query: Default::default(),
					initialize: false,
					depth: None,
					recommend_shallow: true,
					remote: false,
					fetch: true,
					strategy: None,
					initialize_only_active: false,
					reflog_committer: None,
				},
				&configuration,
				&transfer,
				&(),
			)
			.await
			.unwrap_err();

		assert!(
			error
				.to_string()
				.contains("stop after observing serialized configuration")
		);
		assert_eq!(observations.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn sync_revalidates_unchanged_declarations_before_reporting_success() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().canonicalize().unwrap().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		let modules =
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n";
		std::fs::write(worktree.join(".gitmodules"), modules).unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\
			 [submodule \"one\"]\n\tactive = true\n\turl = https://example.invalid/one\n",
		)
		.unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let configuration = Arc::new(
			WorktreeConfiguration::new(
				common.try_clone().unwrap(),
				git.try_clone().unwrap(),
				&git_dir,
				&git_dir,
			)
			.with_sync_declaration_validation_pause(Arc::clone(&entered), Arc::clone(&release)),
		);
		let effective = configuration.reload().await.unwrap();
		let context = Arc::new(
			SubmoduleContext::new(
				RepositoryLayout {
					worktree_root: Some(worktree.clone()),
					git_dir: git_dir.clone(),
					common_dir: git_dir.clone(),
				},
				common,
				git,
				Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
				ConfigViews::new(effective),
				String::new(),
				HashKind::Sha1,
			)
			.unwrap(),
		);
		let running_context = Arc::clone(&context);
		let running_configuration = Arc::clone(&configuration);
		let task = tokio::spawn(async move {
			running_context
				.sync(
					&SyncRequest {
						query: SubmoduleQuery::all(),
					},
					running_configuration.as_ref(),
				)
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			if task.is_finished() {
				break;
			}
			tokio::task::yield_now().await;
		}
		if !entered.load(Ordering::SeqCst) {
			panic!(
				"sync completed before terminal validation: {:?}",
				task.await.unwrap()
			);
		}

		let displaced = worktree.join(".gitmodules.displaced");
		std::fs::rename(worktree.join(".gitmodules"), &displaced).unwrap();
		std::fs::write(worktree.join(".gitmodules"), modules).unwrap();
		release.store(true, Ordering::SeqCst);

		let error = task.await.unwrap().unwrap_err();
		assert!(
			error
				.to_string()
				.contains("config changed after set-url was planned"),
			"unexpected error: {error}"
		);
		assert!(!git_dir.join("gitana-submodule-set-url").exists());
	}

	#[tokio::test]
	async fn recovery_module_enumeration_does_not_read_a_missing_child_config() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		let module_git = git_dir.join("modules/one");
		let module_worktree = worktree.join("modules/one");
		for directory in [
			git_dir.join("objects"),
			git_dir.join("refs"),
			module_git.join("objects"),
			module_git.join("refs"),
			module_worktree.clone(),
		] {
			std::fs::create_dir_all(directory).unwrap();
		}
		std::fs::write(
			worktree.join(".gitmodules"),
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		std::fs::write(
			module_worktree.join(".git"),
			"gitdir: ../../.git/modules/one\n",
		)
		.unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		let config = "[core]\n\trepositoryformatversion = 0\n";
		std::fs::write(git_dir.join("config"), config).unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let context = SubmoduleContext::new(
			RepositoryLayout {
				worktree_root: Some(worktree.clone()),
				git_dir: git_dir.clone(),
				common_dir: git_dir,
			},
			common,
			git,
			Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
			ConfigViews::new(GitConfig::parse(config).unwrap()),
			String::new(),
			HashKind::Sha1,
		)
		.unwrap();

		let modules = context
			.initialized_recovery_modules(&SubmoduleQuery::all())
			.await
			.unwrap();
		assert_eq!(modules.len(), 1);
		assert_eq!(modules[0].path, "modules/one");
		assert!(!module_git.join("config").exists());
	}

	#[tokio::test]
	async fn cancelled_initialization_retains_the_mutation_lock_until_config_publication_finishes() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			worktree.join(".gitmodules"),
			"[submodule \"one\"]\n\tpath = modules/one\n\turl = https://example.invalid/one\n",
		)
		.unwrap();
		let mut index = Index::<Sha1>::new();
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o160000,
			oid: ObjectId::from_hex(&"1".repeat(40)).unwrap(),
			stage: 0,
			assume_valid: false,
			skip_worktree: false,
			intent_to_add: false,
			path: "modules/one".to_owned(),
		});
		std::fs::write(git_dir.join("index"), index.write_v4()).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();

		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let configuration = Arc::new(
			WorktreeConfiguration::new(
				common.try_clone().unwrap(),
				git.try_clone().unwrap(),
				&git_dir,
				&git_dir,
			)
			.with_apply_init_pause(Arc::clone(&entered), Arc::clone(&release)),
		);
		let initial = configuration.reload().await.unwrap();
		let context = Arc::new(
			SubmoduleContext::new(
				RepositoryLayout {
					worktree_root: Some(worktree.clone()),
					git_dir: git_dir.clone(),
					common_dir: git_dir.clone(),
				},
				common,
				git,
				Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap(),
				ConfigViews::new(initial),
				String::new(),
				HashKind::Sha1,
			)
			.unwrap(),
		);
		let running_context = Arc::clone(&context);
		let running_configuration = Arc::clone(&configuration);
		let task = tokio::spawn(async move {
			running_context
				.init(&InitRequest::default(), running_configuration.as_ref())
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(entered.load(Ordering::SeqCst));

		task.abort();
		assert!(task.await.unwrap_err().is_cancelled());
		let retry_context = Arc::clone(&context);
		let retry_configuration = Arc::clone(&configuration);
		let retry = tokio::spawn(async move {
			retry_context
				.init(&InitRequest::default(), retry_configuration.as_ref())
				.await
		});
		for _ in 0..100 {
			tokio::task::yield_now().await;
		}
		assert!(
			!retry.is_finished(),
			"the read-only planning pass must wait for the detached config publisher"
		);
		release.store(true, Ordering::SeqCst);
		retry.await.unwrap().unwrap();
		assert!(
			std::fs::read_to_string(git_dir.join("config"))
				.unwrap()
				.contains("url = https://example.invalid/one")
		);
	}

	#[tokio::test]
	async fn module_deinit_rejects_a_same_content_config_target_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let module = temporary.path().join("module");
		std::fs::create_dir(&module).unwrap();
		std::fs::create_dir_all(temporary.path().join("work/module")).unwrap();
		let config = module.join("config");
		let bytes = "[core]\n\tworktree = ../work/module\n";
		std::fs::write(&config, bytes).unwrap();
		let module_dir = Dir::open_ambient_dir(&module, ambient_authority()).unwrap();
		let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let configuration = WorktreeConfiguration::new(
			root.try_clone().unwrap(),
			root,
			temporary.path(),
			temporary.path(),
		);
		let mounted_worktree =
			Dir::open_ambient_dir(temporary.path().join("work/module"), ambient_authority()).unwrap();
		let transition = configuration
			.plan_module_deinit(
				module_dir.try_clone().unwrap(),
				&module,
				"../work/module",
				Some(mounted_worktree),
			)
			.await
			.unwrap();
		std::fs::rename(&config, module.join("planned-config")).unwrap();
		std::fs::write(&config, bytes).unwrap();

		let result = configuration
			.reserve_module_deinit(
				module_dir,
				&module,
				"../work/module",
				&transition,
				mutation_lease(),
			)
			.await;
		assert!(result.is_err());
		assert_eq!(std::fs::read_to_string(config).unwrap(), bytes);
		assert_eq!(
			std::fs::read_to_string(module.join("planned-config")).unwrap(),
			bytes
		);
	}

	#[tokio::test]
	async fn module_deinit_revalidates_a_worktree_alias_inside_config_publication() {
		let temporary = tempfile::tempdir().unwrap();
		let module = temporary.path().join("module");
		let mount = temporary.path().join("work/module");
		let foreign = temporary.path().join("foreign");
		std::fs::create_dir(&module).unwrap();
		std::fs::create_dir_all(&mount).unwrap();
		std::fs::create_dir(&foreign).unwrap();
		symlink("work/module", temporary.path().join("alias")).unwrap();
		let config = module.join("config");
		let bytes = "[core]\n\trepositoryformatversion = 0\n\tworktree = ../alias\n";
		std::fs::write(&config, bytes).unwrap();
		let module_dir = Dir::open_ambient_dir(&module, ambient_authority()).unwrap();
		let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let configuration = Arc::new(
			WorktreeConfiguration::new(
				root.try_clone().unwrap(),
				root,
				temporary.path(),
				temporary.path(),
			)
			.with_module_deinit_publication_pause(Arc::clone(&entered), Arc::clone(&release)),
		);
		let mounted_worktree = Dir::open_ambient_dir(&mount, ambient_authority()).unwrap();
		let transition = configuration
			.plan_module_deinit(
				module_dir.try_clone().unwrap(),
				&module,
				"../work/module",
				Some(mounted_worktree),
			)
			.await
			.unwrap();
		assert!(transition.worktree_attachment.is_some());
		let publication = configuration
			.reserve_module_deinit(
				module_dir.try_clone().unwrap(),
				&module,
				"../work/module",
				&transition,
				mutation_lease(),
			)
			.await
			.unwrap()
			.unwrap();
		configuration
			.prepare_module_deinit(
				module_dir.try_clone().unwrap(),
				&module,
				"../work/module",
				&transition,
				&publication,
				mutation_lease(),
			)
			.await
			.unwrap();

		let running_configuration = Arc::clone(&configuration);
		let running_module = module.clone();
		let running_transition = transition.clone();
		let running_publication = publication.clone();
		let task = tokio::spawn(async move {
			running_configuration
				.apply_module_deinit(
					module_dir,
					&running_module,
					"../work/module",
					&running_transition,
					Some(&running_publication),
					mutation_lease(),
				)
				.await
		});
		for _ in 0..100_000 {
			if entered.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(entered.load(Ordering::SeqCst));
		std::fs::remove_file(temporary.path().join("alias")).unwrap();
		symlink("foreign", temporary.path().join("alias")).unwrap();
		release.store(true, Ordering::SeqCst);

		let error = task.await.unwrap().unwrap_err();
		let rendered = format!("{error:#}");
		assert!(rendered.contains("core.worktree"), "{rendered}");
		assert_eq!(std::fs::read_to_string(config).unwrap(), bytes);
		assert!(module.join("config.lock").is_file());
	}

	#[tokio::test]
	async fn superproject_deinit_rejects_a_config_symlink_retarget_to_the_same_inode() {
		let temporary = tempfile::tempdir().unwrap();
		let common = temporary.path().join("common");
		std::fs::create_dir(&common).unwrap();
		let original = common.join("original-config");
		let foreign = common.join("foreign-config");
		let bytes = "[submodule \"one\"]\n\turl = ../source\n";
		std::fs::write(&original, bytes).unwrap();
		std::fs::hard_link(&original, &foreign).unwrap();
		symlink("original-config", common.join("config")).unwrap();
		let common_dir = Dir::open_ambient_dir(&common, ambient_authority()).unwrap();
		let configuration = WorktreeConfiguration::new(
			common_dir.try_clone().unwrap(),
			common_dir,
			&common,
			&common,
		);
		let transition = configuration
			.plan_superproject_deinit("one", None, temporary.path())
			.await
			.unwrap();
		std::fs::remove_file(common.join("config")).unwrap();
		symlink("foreign-config", common.join("config")).unwrap();

		let result = configuration
			.reserve_superproject_deinit(&transition, mutation_lease())
			.await;
		assert!(result.is_err());
		assert_eq!(std::fs::read_to_string(original).unwrap(), bytes);
		assert_eq!(std::fs::read_to_string(foreign).unwrap(), bytes);
	}

	#[tokio::test]
	async fn initialization_edits_the_retained_common_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let original = temporary.path().join("common");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&original).unwrap();
		std::fs::write(
			original.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		let common = Dir::open_ambient_dir(&original, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(common, git, &original, &original);

		std::fs::rename(&original, &retained).unwrap();
		std::fs::create_dir(&original).unwrap();
		std::fs::write(
			original.join("config"),
			"[core]\n\trepositoryformatversion = 0\n[foreign]\n\tvalue = true\n",
		)
		.unwrap();
		let reloaded = configuration.reload().await.unwrap();
		assert_eq!(reloaded.get_raw("foreign", None, "value"), None);

		let result = configuration
			.apply_init(
				&[InitConfigUpdate {
					name: "one".to_owned(),
					activate: true,
					url_if_absent: Some("../source".to_owned()),
					update_if_absent: Some("checkout".to_owned()),
				}],
				&[],
				mutation_lease(),
			)
			.await
			.unwrap();

		assert_eq!(result.registered_urls, ["one"]);
		let retained_config = std::fs::read_to_string(retained.join("config")).unwrap();
		assert!(retained_config.contains("[submodule \"one\"]"));
		assert!(retained_config.contains("url = ../source"));
		let replacement_config = std::fs::read_to_string(original.join("config")).unwrap();
		assert!(replacement_config.contains("[foreign]"));
		assert!(!replacement_config.contains("[submodule \"one\"]"));
	}

	#[tokio::test]
	async fn initialization_persists_ordered_active_pathspecs_without_module_updates() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::write(
			common_path.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(common, git, &common_path, &common_path);

		let result = configuration
			.apply_init(
				&[],
				&["modules/a".to_owned(), ":(exclude)modules/b".to_owned()],
				mutation_lease(),
			)
			.await
			.unwrap();

		assert_eq!(result, InitConfigResult::default());
		let config = configuration.reload().await.unwrap();
		assert_eq!(
			config.get_all_raw("submodule", None, "active"),
			vec![Some("modules/a"), Some(":(exclude)modules/b")]
		);
	}

	#[tokio::test]
	async fn module_reads_use_the_retained_repository_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let module_path = temporary.path().join("module");
		let retained_path = temporary.path().join("retained-module");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::create_dir(&module_path).unwrap();
		std::fs::write(
			common_path.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		std::fs::write(
			module_path.join("config"),
			"[core]\n\trepositoryformatversion = 0\n[retained]\n\tvalue = true\n",
		)
		.unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let module_hash = module.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(common, git, &common_path, &common_path);

		std::fs::rename(&module_path, &retained_path).unwrap();
		std::fs::create_dir(&module_path).unwrap();
		std::fs::write(
			module_path.join("config"),
			"[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n[foreign]\n\tvalue = true\n",
		)
		.unwrap();

		let loaded = configuration
			.load_module_config(module, &module_path)
			.await
			.unwrap();
		assert_eq!(
			loaded.get_raw("retained", None, "value"),
			Some(Some("true"))
		);
		assert_eq!(loaded.get_raw("foreign", None, "value"), None);
		assert_eq!(
			configuration
				.module_hash_kind(module_hash, &module_path)
				.await
				.unwrap(),
			HashKind::Sha1
		);
	}

	#[tokio::test]
	async fn module_worktree_edit_rolls_back_only_its_own_published_config() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let module_path = temporary.path().join("module");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::create_dir(&module_path).unwrap();
		std::fs::write(
			common_path.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		std::fs::write(module_path.join("config"), "[core]\n\tbare = true\n").unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(common, git, &common_path, &common_path);
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let edit = configuration
			.set_module_worktree(
				module.try_clone().unwrap(),
				&module_path.join("config"),
				"../../work",
				mutation_lease(),
			)
			.await
			.unwrap();
		assert!(
			std::fs::read_to_string(module_path.join("config"))
				.unwrap()
				.contains("worktree = ../../work")
		);

		configuration
			.rollback_module_worktree(module, &module_path.join("config"), edit, mutation_lease())
			.await
			.unwrap();
		assert_eq!(
			std::fs::read_to_string(module_path.join("config")).unwrap(),
			"[core]\n\tbare = true\n"
		);
	}

	#[tokio::test]
	async fn module_worktree_rollback_preserves_a_same_content_replacement() {
		use std::os::unix::fs::MetadataExt as _;

		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path().join("common");
		let module_path = temporary.path().join("module");
		let displaced = temporary.path().join("published-config");
		std::fs::create_dir(&common_path).unwrap();
		std::fs::create_dir(&module_path).unwrap();
		std::fs::write(
			common_path.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();
		std::fs::write(module_path.join("config"), "[core]\n\tbare = true\n").unwrap();
		let common = Dir::open_ambient_dir(&common_path, ambient_authority()).unwrap();
		let git = common.try_clone().unwrap();
		let configuration = WorktreeConfiguration::new(common, git, &common_path, &common_path);
		let module = Dir::open_ambient_dir(&module_path, ambient_authority()).unwrap();
		let edit = configuration
			.set_module_worktree(
				module.try_clone().unwrap(),
				&module_path.join("config"),
				"../../work",
				mutation_lease(),
			)
			.await
			.unwrap();
		let after = std::fs::read(module_path.join("config")).unwrap();
		std::fs::rename(module_path.join("config"), &displaced).unwrap();
		std::fs::write(module_path.join("config"), &after).unwrap();
		let replacement_inode = std::fs::metadata(module_path.join("config")).unwrap().ino();

		let error = configuration
			.rollback_module_worktree(module, &module_path.join("config"), edit, mutation_lease())
			.await
			.unwrap_err();
		assert!(error.to_string().contains("config target changed"));
		assert_eq!(std::fs::read(module_path.join("config")).unwrap(), after);
		assert_eq!(
			std::fs::metadata(module_path.join("config")).unwrap().ino(),
			replacement_inode
		);
	}
}
