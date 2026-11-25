//! git's `absorbgitdirs`, as its deinit runs it on the submodules inside a checkout before checking
//! it: each nested submodule whose `.git` is a directory has that directory moved into its parent
//! repository's `modules/<name>`, and a gitfile left in its place (probed vs git 2.55).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::{
	AbsorbedGitDir, ConfigurationProvider, SubmoduleDeclaration, SubmoduleError, validate_name,
};

/// One repository whose submodules are being absorbed: its checkout and its git directory, and the
/// gitlinks still to look at.
struct Level {
	work: Dir,
	work_path: PathBuf,
	git: Dir,
	git_path: PathBuf,
	/// The checkout's path from the superproject's work tree, for messages.
	display: String,
	/// Gitlink paths still to look at, last first.
	gitlinks: Vec<String>,
	/// Each declared path's declaration name, from the checkout's `.gitmodules`.
	names: HashMap<String, String>,
}

/// Absorb the `.git` directories of the submodules inside the checkout `work` (at `work_path`, shown
/// as `display`), whose repository is `git` (at `git_path`), depth first as git does: a nested
/// submodule's directory moves to `<its parent's git dir>/modules/<name>`, its gitfile names it there,
/// and its `core.worktree` names the mount back. A gitfile is left alone (git doesn't move a
/// repository it names elsewhere), but the submodules inside it are looked at too.
///
/// It refuses, with git's messages, a nested `.git` directory with no `.gitmodules` declaration to
/// name it, or one whose destination isn't empty. What's moved before a refusal stays moved, as in
/// git. Neither step is journaled: git writes the gitfile and `core.worktree` after the move too.
pub(crate) async fn absorb_git_dirs<H: HashAlgorithm, C: ConfigurationProvider>(
	work: &Dir,
	work_path: &Path,
	git: &Dir,
	git_path: &Path,
	display: &str,
	configuration: &C,
) -> Result<Vec<AbsorbedGitDir>, SubmoduleError> {
	let mut absorbed = Vec::new();
	let mut stack = vec![
		level::<H>(
			clone(work, work_path)?,
			work_path.to_path_buf(),
			clone(git, git_path)?,
			git_path.to_path_buf(),
			display.to_owned(),
		)
		.await?,
	];
	while let Some(top) = stack.last_mut() {
		let Some(path) = top.gitlinks.pop() else {
			stack.pop();
			continue;
		};
		let marker = Path::new(&path).join(".git");
		let shown = format!("{}/{path}", top.display);
		let nested_path = top.work_path.join(&path);
		let child = match top.work.symlink_metadata(&marker) {
			Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
				let name = top
					.names
					.get(&path)
					.cloned()
					// git names the path within its parent here (probed vs git 2.55).
					.ok_or_else(|| SubmoduleError::AbsorbUnnamed(path.clone()))?;
				validate_name(&name)?;
				let destination = Path::new("modules").join(&name);
				let destination_path = top.git_path.join(&destination);
				let from = nested_path.join(".git");
				match top.git.read_dir(&destination) {
					Ok(mut entries) => {
						if entries.next().is_some() {
							return Err(SubmoduleError::AbsorbBlocked {
								from,
								to: destination_path,
							});
						}
						top
							.git
							.remove_dir(&destination)
							.map_err(io(&destination_path))?;
					}
					Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
					Err(source) => {
						return Err(SubmoduleError::Io {
							path: destination_path,
							source,
						});
					}
				}
				if let Some(parent) = destination.parent() {
					top
						.git
						.create_dir_all(parent)
						.map_err(io(&top.git_path.join(parent)))?;
				}
				top
					.work
					.rename(&marker, &top.git, &destination)
					.map_err(io(&from))?;
				top
					.work
					.write(
						&marker,
						format!("gitdir: {}\n", relative(&nested_path, &destination_path)),
					)
					.map_err(io(&from))?;
				let moved = top
					.git
					.open_dir(&destination)
					.map_err(io(&destination_path))?;
				let lease = crate::update_operation::try_acquire_submodule_config_mutation_lease(
					&moved,
					&destination_path,
				)?;
				configuration
					.set_module_worktree(
						clone(&moved, &destination_path)?,
						&destination_path,
						&relative(&destination_path, &nested_path),
						lease,
					)
					.await?;
				absorbed.push(AbsorbedGitDir {
					path: shown.clone(),
					from,
					to: destination_path.clone(),
				});
				Some((moved, destination_path))
			}
			Ok(meta) if meta.is_file() => {
				gitfile_target(&top.work, &marker, &nested_path).and_then(|target| {
					Dir::open_ambient_dir(&target, ambient_authority())
						.ok()
						.map(|git| (git, target))
				})
			}
			_ => None,
		};
		if let Some((git, git_path)) = child {
			let work = top.work.open_dir(&path).map_err(io(&nested_path))?;
			stack.push(level::<H>(work, nested_path, git, git_path, shown).await?);
		}
	}
	Ok(absorbed)
}

/// The repository `git` checked out at `work`, with its gitlinks and their declared names.
async fn level<H: HashAlgorithm>(
	work: Dir,
	work_path: PathBuf,
	git: Dir,
	git_path: PathBuf,
	display: String,
) -> Result<Level, SubmoduleError> {
	let tree = WorkTree::new(
		Repository::<_, H>::new(ObjectStore::new(LocalFileStore::from_dir(clone(
			&git, &git_path,
		)?))),
		CapWorkDir::from_dir(clone(&work, &work_path)?),
		&git_path,
	);
	let mut gitlinks: Vec<String> = tree
		.load_index()
		.await?
		.entries
		.iter()
		.filter(|entry| entry.stage == 0 && entry.mode == 0o160000)
		.filter_map(|entry| entry.path.as_utf8().map(str::to_owned))
		.collect();
	gitlinks.reverse();
	let names = match work.read(".gitmodules") {
		Ok(bytes) => SubmoduleDeclaration::parse_all(&String::from_utf8_lossy(&bytes))?
			.into_iter()
			.map(|declaration| (declaration.path, declaration.name))
			.collect(),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
		Err(source) => {
			return Err(SubmoduleError::Io {
				path: work_path.join(".gitmodules"),
				source,
			});
		}
	};
	Ok(Level {
		work,
		work_path,
		git,
		git_path,
		display,
		gitlinks,
		names,
	})
}

/// The git directory a gitfile at `marker` (in `work`, the submodule at `nested_path`) names.
fn gitfile_target(work: &Dir, marker: &Path, nested_path: &Path) -> Option<PathBuf> {
	let text = work.read_to_string(marker).ok()?;
	let target = text.strip_prefix("gitdir:")?.trim();
	(!target.is_empty()).then(|| nested_path.join(target))
}

/// `to` from the folder `from`, both absolute, as `/`-separated text (`..` to go up), as git writes
/// a gitfile's and `core.worktree`'s relative paths.
fn relative(from: &Path, to: &Path) -> String {
	let from: Vec<Component<'_>> = from.components().collect();
	let to: Vec<Component<'_>> = to.components().collect();
	let common = from
		.iter()
		.zip(&to)
		.take_while(|(left, right)| left == right)
		.count();
	let mut parts: Vec<String> = from[common..].iter().map(|_| "..".to_owned()).collect();
	parts.extend(
		to[common..]
			.iter()
			.map(|component| component.as_os_str().to_string_lossy().into_owned()),
	);
	parts.join("/")
}

fn clone(directory: &Dir, path: &Path) -> Result<Dir, SubmoduleError> {
	directory.try_clone().map_err(io(path))
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SubmoduleError + '_ {
	move |source| SubmoduleError::Io {
		path: path.to_path_buf(),
		source,
	}
}

#[cfg(test)]
mod tests {
	use super::relative;
	use std::path::Path;

	#[test]
	fn writes_git_s_relative_paths() {
		assert_eq!(
			relative(
				Path::new("/w/super/sub/inner"),
				Path::new("/w/super/.git/modules/sub/modules/inner")
			),
			"../../.git/modules/sub/modules/inner"
		);
		assert_eq!(
			relative(
				Path::new("/w/super/.git/modules/sub/modules/inner"),
				Path::new("/w/super/sub/inner")
			),
			"../../../../../sub/inner"
		);
	}
}
