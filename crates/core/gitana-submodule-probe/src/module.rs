use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use gitana_config::{GitConfig, GitConfigSource};
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_repository_layout::RepositoryLayout;
use gitana_worktree::{OutsideAttributes, Status, SubmoduleProbe, WorkTree};

use crate::open_dir as open;

/// A repository with a work tree, opened natively as git opens one: its own config (every scope,
/// includes, and the command line's `-c`), excludes and attributes.
pub(crate) struct Module<H: HashAlgorithm> {
	tree: WorkTree<WorktreeFileStore, CapWorkDir, H>,
	excludes: Option<Vec<u8>>,
	layout: RepositoryLayout,
	config: GitConfig,
}

impl<H: HashAlgorithm> Module<H> {
	/// The repository mounted at `mount`, found as git finds it: from the mount's `.git`, a gitfile
	/// (followed wherever it points) or a directory, without looking above the mount. A `.git` that
	/// names no repository is an error, as it's fatal to git's status. Its work tree is the one its
	/// `core.worktree` names, as git takes it, unless `mount_is_work_tree`.
	pub(crate) async fn mounted_at(mount: &Path, mount_is_work_tree: bool) -> Result<Self> {
		let layout = if mount_is_work_tree {
			let mut layout = gitana_repository_layout::inspect_source(mount).await;
			if let Ok(layout) = &mut layout {
				layout.worktree_root = Some(mount.to_path_buf());
				layout.marker_root = Some(mount.to_path_buf());
			}
			layout
		} else {
			gitana_repository_layout::inspect_root(mount).await
		}
		.with_context(|| format!("{} isn't a git repository", mount.display()))?;
		Self::open(layout).await
	}

	/// The repository `layout` describes, which must have a work tree.
	async fn open(layout: RepositoryLayout) -> Result<Self> {
		let Some(root) = layout.worktree_root.clone() else {
			bail!("{} has no work tree", layout.git_dir.display());
		};
		let common = open(&layout.common_dir)?;
		let git = open(&layout.git_dir)?;
		let work = open(&root)?;
		// The object format is the repository's own, read from its config file alone, as git reads
		// `extensions.*`; a gitlink records a commit in the superproject's format, so a module must
		// share it.
		let local = gitana_config_native::read_file_at(
			common.try_clone()?,
			Path::new("config"),
			&layout.common_dir.join("config"),
		)
		.await?;
		let local = GitConfigSource::parse(&String::from_utf8_lossy(&local.unwrap_or_default()))
			.with_context(|| format!("{}'s config can't be read", layout.common_dir.display()))?;
		let format = local
			.get_string("extensions", None, "objectformat")
			.unwrap_or("sha1");
		if !format.eq_ignore_ascii_case(H::NAME) {
			bail!(
				"{} uses the {format} object format, not {}",
				layout.common_dir.display(),
				H::NAME
			);
		}
		let config = gitana_config_native::for_worktree_at(
			common.try_clone()?,
			git.try_clone()?,
			&layout.common_dir,
			&layout.git_dir,
		)
		.await?;
		let excludes =
			gitana_config_native::excludes_file_at(&config, work.try_clone()?, &root).await?;
		let attributes =
			gitana_config_native::attributes_files_at(&config, work.try_clone()?, &root).await?;
		let mut repository = Repository::new(ObjectStore::new(WorktreeFileStore::new(common, git)));
		repository.set_effective_config(config.clone());
		let tree = WorkTree::new_located(
			repository,
			CapWorkDir::from_dir(work),
			layout.git_dir.clone(),
			root,
		)
		.with_outside_attributes(OutsideAttributes {
			system: attributes.system,
			global: attributes.global,
		});
		Ok(Self {
			tree,
			excludes,
			layout,
			config,
		})
	}

	pub(crate) fn layout(&self) -> &RepositoryLayout {
		&self.layout
	}

	pub(crate) fn config(&self) -> &GitConfig {
		&self.config
	}

	/// The commit checked out, or `None` while `HEAD` is unborn.
	pub(crate) async fn head(&self) -> Result<Option<ObjectId<H>>> {
		Ok(self.tree.repository().refs().resolve_head().await?)
	}

	/// The repository's status, with `probe` looking inside its own submodules.
	pub(crate) async fn status(self, probe: Arc<dyn SubmoduleProbe<H>>) -> Result<Status> {
		let tree = self.tree.with_submodule_probe(probe);
		Ok(tree.status(self.excludes.as_deref()).await?)
	}
}
