use std::path::{Path, PathBuf};

use anyhow::bail;
use cap_std::fs::Dir;
use gitana_config::GitConfig;
use gitana_object::HashKind;
use gitana_repository::Config;
use gitana_submodule::{ConfigurationProvider, InitConfigResult, InitConfigUpdate, SubmoduleError};

use crate::git_config;

/// Native authority for rebuilding Git's complete effective configuration stack for one worktree.
pub(crate) struct WorktreeConfiguration {
	common: Dir,
	git: Dir,
	common_dir: PathBuf,
	git_dir: PathBuf,
}

impl WorktreeConfiguration {
	pub(crate) fn new(common: Dir, git: Dir, common_dir: &Path, git_dir: &Path) -> Self {
		Self {
			common,
			git,
			common_dir: common_dir.to_owned(),
			git_dir: git_dir.to_owned(),
		}
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

impl ConfigurationProvider for WorktreeConfiguration {
	type ModuleWorktreeEdit = ModuleWorktreeEdit;

	async fn apply_init(
		&self,
		updates: &[InitConfigUpdate],
	) -> Result<InitConfigResult, SubmoduleError> {
		let path = self.common_dir.join("config");
		let common = self.common.try_clone().map_err(|error| {
			SubmoduleError::Configuration(format!("opening {}: {error}", self.common_dir.display()))
		})?;
		let updates = updates.to_vec();
		gitana_config_native::edit_file_at(common, Path::new("config"), &path, move |config| {
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
		})
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

	async fn set_module_worktree(
		&self,
		git_dir: Dir,
		display_path: &Path,
		worktree: &str,
	) -> Result<Self::ModuleWorktreeEdit, SubmoduleError> {
		let worktree = worktree.to_owned();
		let ((before, after), publication) = gitana_config_native::edit_file_at_tracked(
			git_dir,
			Path::new("config"),
			display_path,
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
	) -> Result<(), SubmoduleError> {
		gitana_config_native::edit_file_at_if_current(edit.publication, display_path, move |config| {
			if config.render() != edit.after {
				bail!("module config changed after publishing core.worktree");
			}
			*config = GitConfig::parse(&edit.before)?;
			Ok(())
		})
		.await
		.map_err(|error| SubmoduleError::Configuration(format!("{error:#}")))
	}
}

#[cfg(all(test, unix))]
mod tests {
	use cap_std::ambient_authority;

	use super::*;

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
			.apply_init(&[InitConfigUpdate {
				name: "one".to_owned(),
				activate: true,
				url_if_absent: Some("../source".to_owned()),
				update_if_absent: Some("checkout".to_owned()),
			}])
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
			)
			.await
			.unwrap();
		assert!(
			std::fs::read_to_string(module_path.join("config"))
				.unwrap()
				.contains("worktree = ../../work")
		);

		configuration
			.rollback_module_worktree(module, &module_path.join("config"), edit)
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
			)
			.await
			.unwrap();
		let after = std::fs::read(module_path.join("config")).unwrap();
		std::fs::rename(module_path.join("config"), &displaced).unwrap();
		std::fs::write(module_path.join("config"), &after).unwrap();
		let replacement_inode = std::fs::metadata(module_path.join("config")).unwrap().ino();

		let error = configuration
			.rollback_module_worktree(module, &module_path.join("config"), edit)
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
