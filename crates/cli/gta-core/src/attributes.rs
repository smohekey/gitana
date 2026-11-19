//! The attributes files git reads from outside the repository, found for the worktree library.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_config::GitConfig;
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::HashAlgorithm;
use gitana_submodule::SubmoduleContext;
use gitana_worktree::{OutsideAttributes, WorkTree};

/// git's system attributes file.
const SYSTEM_FILE: &str = "/etc/gitattributes";

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

/// The system and global attributes files, as git reads them: `/etc/gitattributes`, unless
/// `GIT_ATTR_NOSYSTEM` is true, and `core.attributesFile`, by default
/// `$XDG_CONFIG_HOME/git/attributes` (or `~/.config/git/attributes`). A leading `~` in the setting is
/// the home folder, and a relative path is under `root`, the worktree's root folder, where git runs.
/// A file that's absent, or a folder, adds nothing; one that can't be read adds nothing either, with
/// git's warning.
pub(crate) async fn outside_attributes(
	config: &GitConfig,
	root: &Path,
) -> Result<OutsideAttributes> {
	let system = system_file().await?;
	let global = match global_file(config)? {
		Some(path) if path.is_absolute() => read(&path).await,
		Some(path) => read(&root.join(path)).await,
		None => None,
	};
	Ok(OutsideAttributes { system, global })
}

/// [`outside_attributes`] for a worktree already open as `worktree`, whose root folder was
/// `worktree_root`: a global file inside the worktree is read through the open folder, so one moved
/// aside (as `submodule deinit` moves a checkout) is still found.
pub(crate) async fn outside_attributes_at(
	config: &GitConfig,
	worktree: cap_std::fs::Dir,
	worktree_root: &Path,
) -> Result<OutsideAttributes> {
	let system = system_file().await?;
	let global = match global_file(config)? {
		Some(path) if path.is_absolute() => {
			match crate::excludes::absolute_path_in_worktree(&path, worktree_root).await {
				Some(relative) => read_at(worktree, relative, path).await,
				None => read(&path).await,
			}
		}
		// Inside the checkout: through the open folder. Leaving it (`../attrs`): by path from the root
		// folder, whose parent is the same even once the checkout has been moved aside.
		Some(path) if stays_inside(&path) => {
			let display = worktree_root.join(&path);
			read_at(worktree, path, display).await
		}
		Some(path) => read(&worktree_root.join(path)).await,
		None => None,
	};
	Ok(OutsideAttributes { system, global })
}

/// Whether the relative `path` stays inside the folder it's relative to, by its components alone.
fn stays_inside(path: &Path) -> bool {
	use std::path::Component;
	let mut depth = 0usize;
	for component in path.components() {
		match component {
			Component::Normal(_) => depth += 1,
			Component::CurDir => {}
			Component::ParentDir => match depth.checked_sub(1) {
				Some(less) => depth = less,
				None => return false,
			},
			Component::RootDir | Component::Prefix(_) => return false,
		}
	}
	true
}

/// The system file, unless `GIT_ATTR_NOSYSTEM` is true.
async fn system_file() -> Result<Option<Vec<u8>>> {
	let nosystem = match std::env::var("GIT_ATTR_NOSYSTEM") {
		Err(_) => false,
		Ok(value) => gitana_config_native::parse_git_bool(&value)
			.ok_or_else(|| anyhow!("bad boolean environment value '{value}' for 'GIT_ATTR_NOSYSTEM'"))?,
	};
	Ok(if nosystem {
		None
	} else {
		read(Path::new(SYSTEM_FILE)).await
	})
}

/// Where `core.attributesFile` points (`~` expanded; a relative path is relative to the worktree's
/// root, where git runs), or git's default; `None` when there's nowhere to look.
fn global_file(config: &GitConfig) -> Result<Option<PathBuf>> {
	// git refuses a valueless `core.attributesFile`, wherever it's set.
	if config
		.get_all_raw("core", None, "attributesfile")
		.iter()
		.any(Option::is_none)
	{
		bail!("missing value for 'core.attributesfile'");
	}
	Ok(match config.get_string("core", None, "attributesfile") {
		Some(configured) => {
			let path = crate::excludes::expand_tilde_in("core.attributesFile", configured)?;
			(!path.as_os_str().is_empty()).then_some(path)
		}
		None => crate::excludes::xdg_config_home().map(|base| base.join("git").join("attributes")),
	})
}

/// The file at `path`, as git's attributes reader takes it.
async fn read(path: &Path) -> Option<Vec<u8>> {
	taken(tokio::fs::read(path).await, path)
}

/// The file at `relative` in `dir` (shown as `display`), as [`read`] takes one. A symlink is
/// followed wherever it points, as git opens the path: through the open folder when it stays inside,
/// and otherwise by config-native's reader, which follows and revalidates it.
async fn read_at(dir: cap_std::fs::Dir, relative: PathBuf, display: PathBuf) -> Option<Vec<u8>> {
	let (inside, path) = match dir.try_clone() {
		Ok(inside) => (inside, relative.clone()),
		Err(error) => return taken(Err(error), &display),
	};
	let result = tokio::task::spawn_blocking(move || inside.read(path))
		.await
		.unwrap_or_else(|error| Err(std::io::Error::other(error)));
	match result {
		// cap-std refuses a path that leads out of the folder (no OS error): follow it.
		Err(error)
			if error.kind() == std::io::ErrorKind::PermissionDenied && error.raw_os_error().is_none() =>
		{
			let result = match gitana_config_native::read_file_at(dir, &relative, &display).await {
				Ok(Some(bytes)) => Ok(bytes),
				Ok(None) => Err(std::io::ErrorKind::NotFound.into()),
				Err(error) => Err(std::io::Error::other(error.to_string())),
			};
			taken(result, &display)
		}
		result => taken(result, &display),
	}
}

/// A read's bytes, as git's attributes reader takes them: absent, a folder, or a path through a file
/// adds nothing; another failure adds nothing either, with git's warning.
fn taken(result: std::io::Result<Vec<u8>>, path: &Path) -> Option<Vec<u8>> {
	match result {
		Ok(bytes) => Some(bytes),
		Err(error) => {
			use std::io::ErrorKind::{IsADirectory, NotADirectory, NotFound};
			if !matches!(error.kind(), NotFound | NotADirectory | IsADirectory) {
				eprintln!(
					"warning: unable to access '{}': {}",
					path.display(),
					strerror(&error)
				);
			}
			None
		}
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
