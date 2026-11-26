use std::error::Error;
use std::future::Future;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use gitana_config::GitConfig;
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_object_store::ObjectStore;
use gitana_path::GitPath;
use gitana_repository::{Repository, RepositoryError};
use gitana_repository_layout::RepositoryLayout;
use gitana_worktree::{IgnoreSubmodules, Status, SubmoduleProbe, SubmoduleReport, WorkTree};
use tokio::sync::OnceCell;

use crate::{Declarations, Module, native_path, open_dir};

/// Looks inside the submodules of one repository's work tree, natively, as git's `status` does.
/// Attach it to that repository's [`WorkTree`] with
/// [`with_submodule_probe`](WorkTree::with_submodule_probe); it attaches one of its own to each
/// submodule's, for the submodules inside.
///
/// Build one per status: it reads `.gitmodules` once, on the first gitlink it's asked about, and
/// keeps it. The warnings git would print along the way are kept for the caller to print
/// ([`Self::warnings`]).
///
/// It has no `Debug`, since the config it holds can carry secrets.
pub struct NativeSubmoduleProbe<H> {
	layout: RepositoryLayout,
	config: GitConfig,
	declarations: OnceCell<Declarations>,
	warnings: Arc<Mutex<Vec<String>>>,
	/// Open each submodule with its mount as its work tree, whatever its `core.worktree` says.
	mounts_as_work_trees: bool,
	hash: PhantomData<fn() -> H>,
}

impl<H: HashAlgorithm> NativeSubmoduleProbe<H> {
	/// A probe for the submodules of the repository `layout` describes, which must have a work
	/// tree, and whose effective config (every scope, and the command line's `-c`) is `config`.
	pub fn new(layout: RepositoryLayout, config: GitConfig) -> Self {
		Self {
			layout,
			config,
			declarations: OnceCell::new(),
			warnings: Arc::new(Mutex::new(Vec::new())),
			mounts_as_work_trees: false,
			hash: PhantomData,
		}
	}

	/// This probe, opening each submodule (and the submodules inside it) with its mount as its work
	/// tree, whatever the submodule's `core.worktree` says. For a caller that has moved a work tree
	/// whose submodules' `core.worktree` still names where it was, as deinit's last check before it
	/// retires a checkout does. A submodule's `core.worktree` names its mount anyway, unless it's been
	/// set by hand.
	pub fn with_mounts_as_work_trees(mut self) -> Self {
		self.mounts_as_work_trees = true;
		self
	}

	/// The warnings git would print during the status, each once, without git's `warning: `
	/// prefix: about `.gitmodules` entries it skips or values it ignores, here or in a submodule.
	pub fn warnings(&self) -> Vec<String> {
		self
			.warnings
			.lock()
			.map(|warnings| warnings.clone())
			.unwrap_or_default()
	}

	/// A probe for a submodule's own submodules, keeping its warnings with this one's.
	fn nested(&self, layout: RepositoryLayout, config: GitConfig) -> Self {
		Self {
			warnings: Arc::clone(&self.warnings),
			mounts_as_work_trees: self.mounts_as_work_trees,
			..Self::new(layout, config)
		}
	}

	fn warn(&self, new: Vec<String>) {
		if let Ok(mut warnings) = self.warnings.lock() {
			for warning in new {
				if !warnings.contains(&warning) {
					warnings.push(warning);
				}
			}
		}
	}

	fn root(&self) -> Result<&Path> {
		self
			.layout
			.worktree_root
			.as_deref()
			.ok_or_else(|| anyhow!("{} has no work tree", self.layout.git_dir.display()))
	}

	async fn inspect(
		&self,
		path: &GitPath,
		recorded: ObjectId<H>,
		flag: Option<IgnoreSubmodules>,
	) -> Result<Option<SubmoduleReport>> {
		// With the flag given, git's status reads neither `.gitmodules` nor the submodule's setting.
		let ignore = match flag {
			Some(_) => IgnoreSubmodules::None,
			None => self.setting(path).await?,
		};
		let mut report = SubmoduleReport {
			ignore,
			..SubmoduleReport::default()
		};
		let effective = flag.unwrap_or(ignore);
		if effective == IgnoreSubmodules::All {
			return Ok(Some(report));
		}
		// A deleted or replaced mount, or one with no `.git` (an uninitialized submodule), has
		// nothing inside to report.
		let mount = native_path(self.root()?, path);
		if !tokio::fs::symlink_metadata(&mount)
			.await
			.is_ok_and(|meta| meta.is_dir())
		{
			return Ok(Some(report));
		}
		match tokio::fs::symlink_metadata(mount.join(".git")).await {
			Ok(_) => {}
			Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Some(report)),
			Err(error) => {
				return Err(error).with_context(|| format!("couldn't look at {}", mount.display()));
			}
		}
		let module = Module::<H>::mounted_at(&mount, self.mounts_as_work_trees).await?;
		// An unborn `HEAD` isn't new commits: git counts the module's staged files as modified
		// content instead (probed vs git 2.55).
		report.new_commits = module.head().await?.is_some_and(|head| head != recorded);
		if effective == IgnoreSubmodules::Dirty {
			return Ok(Some(report));
		}
		let nested = Arc::new(self.nested(module.layout().clone(), module.config().clone()));
		let status = module.status(nested).await?;
		(report.modified_content, report.untracked_content) = content(&status);
		Ok(Some(report))
	}

	/// The submodule's own ignore setting: `submodule.<name>.ignore` from the config, else from
	/// `.gitmodules`, else `diff.ignoreSubmodules`, else `none`. A gitlink no declaration names has
	/// no name, so only `diff.ignoreSubmodules` can apply to it.
	async fn setting(&self, path: &GitPath) -> Result<IgnoreSubmodules> {
		let declarations = self
			.declarations
			.get_or_try_init(|| self.read_declarations())
			.await?;
		if let Some(name) = declarations.name(path) {
			match self.config.get_raw("submodule", Some(name), "ignore") {
				Some(Some(value)) => return flag_value(value),
				Some(None) => bail!("missing value for 'submodule.{name}.ignore'"),
				None => {}
			}
			if let Some(ignore) = declarations.ignore(name) {
				return Ok(ignore);
			}
		}
		// Every entry is checked, as git checks them; `submodule.<name>.ignore` above is last-value-only,
		// as git reads it (probed vs git 2.55).
		Ok(IgnoreSubmodules::from_diff_config(&self.config)?.unwrap_or_default())
	}

	async fn read_declarations(&self) -> Result<Declarations> {
		let Some(bytes) = self.gitmodules().await? else {
			return Ok(Declarations::default());
		};
		let mut warnings = Vec::new();
		let declarations = Declarations::parse(&String::from_utf8_lossy(&bytes), &mut warnings);
		self.warn(warnings);
		declarations
	}

	/// The `.gitmodules` git's status reads: the work tree's, if the file is there (a symbolic link
	/// is followed), else the index's, else HEAD's.
	async fn gitmodules(&self) -> Result<Option<Vec<u8>>> {
		let root = self.root()?;
		let file = root.join(".gitmodules");
		match tokio::fs::read(&file).await {
			Ok(bytes) => return Ok(Some(bytes)),
			Err(error) if error.kind() == ErrorKind::NotFound => {}
			Err(error) => {
				return Err(error).with_context(|| format!("couldn't read {}", file.display()));
			}
		}
		let tree: WorkTree<_, _, H> = WorkTree::new_located(
			Repository::new(ObjectStore::new(WorktreeFileStore::new(
				open_dir(&self.layout.common_dir)?,
				open_dir(&self.layout.git_dir)?,
			))),
			CapWorkDir::from_dir(open_dir(root)?),
			self.layout.git_dir.clone(),
			root.to_path_buf(),
		);
		let index = tree.load_index().await?;
		if let Some(entry) = index
			.entries
			.iter()
			.find(|entry| entry.stage == 0 && entry.path == ".gitmodules")
		{
			return Ok(Some(tree.repository().read_blob(entry.oid).await?));
		}
		if tree.repository().refs().resolve_head().await?.is_none() {
			return Ok(None);
		}
		match tree.repository().rev_parse("HEAD:.gitmodules").await {
			Ok(blob) => Ok(Some(tree.repository().read_blob(blob).await?)),
			Err(RepositoryError::MissingTreePath { .. }) => Ok(None),
			Err(error) => Err(error.into()),
		}
	}
}

impl<H: HashAlgorithm> SubmoduleProbe<H> for NativeSubmoduleProbe<H> {
	fn report<'a>(
		&'a self,
		path: &'a GitPath,
		recorded: ObjectId<H>,
		flag: Option<IgnoreSubmodules>,
	) -> Pin<
		Box<
			dyn Future<Output = Result<Option<SubmoduleReport>, Box<dyn Error + Send + Sync>>>
				+ Send
				+ 'a,
		>,
	> {
		Box::pin(async move {
			self
				.inspect(path, recorded, flag)
				.await
				.map_err(|error| format!("{error:#}").into())
		})
	}
}

/// A setting from the config, where git takes only its own spellings (and a bad one is fatal to
/// its status, with this message).
fn flag_value(value: &str) -> Result<IgnoreSubmodules> {
	IgnoreSubmodules::parse(value).ok_or_else(|| anyhow!("bad --ignore-submodules argument: {value}"))
}

/// What a submodule's own status says of its content (probed vs git 2.55): any tracked change is
/// modified content; untracked files are untracked content. A submodule inside it passes on what
/// it has: new commits or modified content as modified content, untracked content as untracked.
fn content(status: &Status) -> (bool, bool) {
	let mut modified = false;
	let mut untracked = !status.untracked.is_empty();
	for entry in &status.changed {
		match entry.submodule {
			Some(inner) if entry.index == ' ' && entry.worktree == 'M' => {
				if inner.new_commits || inner.modified_content {
					modified = true;
				} else {
					untracked |= inner.untracked_content;
				}
			}
			_ => modified = true,
		}
	}
	(modified, untracked)
}
