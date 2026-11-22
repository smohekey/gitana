use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use cap_std::fs::Dir;
use gitana_config::GitConfig;

use crate::excludes_file::{absolute_path_in_worktree, expand_tilde_in, xdg_config_home};

/// git's system attributes file.
const SYSTEM_FILE: &str = "/etc/gitattributes";

/// The attributes files git reads from outside the repository, as found for a worktree: their
/// contents, or `None` for one that's absent, a folder, or not to be read; and git's warnings for
/// one that couldn't be read, for the caller to print where git prints them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttributesFiles {
	/// The system file: `/etc/gitattributes`, unless `GIT_ATTR_NOSYSTEM` is true.
	pub system: Option<Vec<u8>>,
	/// The global file: `core.attributesFile`, by default `$XDG_CONFIG_HOME/git/attributes` (or
	/// `~/.config/git/attributes`).
	pub global: Option<Vec<u8>>,
	/// git's lines for a file that couldn't be read (`warning: unable to access '…': …`), in the
	/// order git gives them.
	pub warnings: Vec<String>,
}

/// The system and global attributes files, as git reads them, for a worktree whose root folder is
/// `root`: `/etc/gitattributes`, unless `GIT_ATTR_NOSYSTEM` is true, and `core.attributesFile`, by
/// default `$XDG_CONFIG_HOME/git/attributes` (or `~/.config/git/attributes`). A leading `~` in the
/// setting is the home folder, and a relative path is under `root`, where git runs. A file that's
/// absent, or a folder, adds nothing; one that can't be read adds nothing either, with git's
/// warning.
pub async fn attributes_files(config: &GitConfig, root: &Path) -> Result<AttributesFiles> {
	let mut warnings = Vec::new();
	let system = system_file(&mut warnings).await?;
	let global = match global_file(config)? {
		Some(path) if path.is_absolute() => read(&path, &mut warnings).await,
		Some(path) => read(&root.join(path), &mut warnings).await,
		None => None,
	};
	Ok(AttributesFiles {
		system,
		global,
		warnings,
	})
}

/// [`attributes_files`] for a worktree already open as `worktree`, whose root folder was
/// `worktree_root`: a global file inside the worktree is read through the open folder, so one moved
/// aside (as `submodule deinit` moves a checkout) is still found.
pub async fn attributes_files_at(
	config: &GitConfig,
	worktree: Dir,
	worktree_root: &Path,
) -> Result<AttributesFiles> {
	let mut warnings = Vec::new();
	let system = system_file(&mut warnings).await?;
	let global = match global_file(config)? {
		Some(path) if path.is_absolute() => {
			match absolute_path_in_worktree(&path, worktree_root).await {
				Some(relative) => read_at(worktree, relative, path, &mut warnings).await,
				None => read(&path, &mut warnings).await,
			}
		}
		// Inside the checkout: through the open folder. Leaving it (`../attrs`): by path from the root
		// folder, whose parent is the same even once the checkout has been moved aside.
		Some(path) if stays_inside(&path) => {
			let display = worktree_root.join(&path);
			read_at(worktree, path, display, &mut warnings).await
		}
		Some(path) => read(&worktree_root.join(path), &mut warnings).await,
		None => None,
	};
	Ok(AttributesFiles {
		system,
		global,
		warnings,
	})
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
async fn system_file(warnings: &mut Vec<String>) -> Result<Option<Vec<u8>>> {
	let nosystem = match crate::environment::var("GIT_ATTR_NOSYSTEM") {
		Err(_) => false,
		Ok(value) => crate::parse_git_bool(&value)
			.ok_or_else(|| anyhow!("bad boolean environment value '{value}' for 'GIT_ATTR_NOSYSTEM'"))?,
	};
	Ok(if nosystem {
		None
	} else {
		read(Path::new(SYSTEM_FILE), warnings).await
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
			let path = expand_tilde_in("core.attributesFile", configured)?;
			(!path.as_os_str().is_empty()).then_some(path)
		}
		None => xdg_config_home().map(|base| base.join("git").join("attributes")),
	})
}

/// The file at `path`, as git's attributes reader takes it.
async fn read(path: &Path, warnings: &mut Vec<String>) -> Option<Vec<u8>> {
	taken(tokio::fs::read(path).await, path, warnings)
}

/// The file at `relative` in `dir` (shown as `display`), as [`read`] takes one. A symlink is
/// followed wherever it points, as git opens the path: through the open folder when it stays inside,
/// and otherwise by [`crate::read_file_at`], which follows and revalidates it.
async fn read_at(
	dir: Dir,
	relative: PathBuf,
	display: PathBuf,
	warnings: &mut Vec<String>,
) -> Option<Vec<u8>> {
	let (inside, path) = match dir.try_clone() {
		Ok(inside) => (inside, relative.clone()),
		Err(error) => return taken(Err(error), &display, warnings),
	};
	let result = tokio::task::spawn_blocking(move || inside.read(path))
		.await
		.unwrap_or_else(|error| Err(std::io::Error::other(error)));
	match result {
		// cap-std refuses a path that leads out of the folder (no OS error): follow it.
		Err(error)
			if error.kind() == std::io::ErrorKind::PermissionDenied && error.raw_os_error().is_none() =>
		{
			let result = match crate::read_file_at(dir, &relative, &display).await {
				Ok(Some(bytes)) => Ok(bytes),
				Ok(None) => Err(std::io::ErrorKind::NotFound.into()),
				Err(error) => Err(std::io::Error::other(error.to_string())),
			};
			taken(result, &display, warnings)
		}
		result => taken(result, &display, warnings),
	}
}

/// A read's bytes, as git's attributes reader takes them: absent, a folder, or a path through a file
/// adds nothing; another failure adds nothing either, with git's warning added to `warnings`.
fn taken(
	result: std::io::Result<Vec<u8>>,
	path: &Path,
	warnings: &mut Vec<String>,
) -> Option<Vec<u8>> {
	match result {
		Ok(bytes) => Some(bytes),
		Err(error) => {
			use std::io::ErrorKind::{IsADirectory, NotADirectory, NotFound};
			if !matches!(error.kind(), NotFound | NotADirectory | IsADirectory) {
				warnings.push(format!(
					"warning: unable to access '{}': {}",
					path.display(),
					strerror(&error)
				));
			}
			None
		}
	}
}

/// `error`'s message as C's `strerror` gives it, without Rust's ` (os error N)`.
fn strerror(error: &std::io::Error) -> String {
	let message = error.to_string();
	match error.raw_os_error() {
		Some(code) => message
			.strip_suffix(&format!(" (os error {code})"))
			.map(str::to_owned)
			.unwrap_or(message),
		None => message,
	}
}
