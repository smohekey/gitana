//! Runtime → compile-time hash-algorithm dispatch.
//!
//! A repository's object hash is a runtime fact (read from `.git/config` or negotiated
//! over the wire), but the engine is generic over a compile-time `H`. This module is the
//! single bridge: [`detect_algorithm_at`] reads the runtime [`HashKind`], and the
//! [`on_repo`]/[`on_worktree`] dispatchers pick the matching `H` and hand a concrete
//! `Repository<_, H>` / `WorkTree<_, H>` to a command whose body is written once, generic
//! over `H`. Adding a third algorithm later touches only this file.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::{Result, anyhow, bail};
use cap_std::fs::Dir;
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::repo::{self, RepositoryLayout};
use crate::{Backend, RepositoryLayoutIdentity, RetainedCommandDirectory, WorkDir};

/// Read the object format through a common-directory capability that has already been identity
/// checked. The path is retained only for diagnostics and supported config-symlink resolution.
pub(crate) async fn detect_algorithm_at(common: &Dir, common_dir: &Path) -> Result<HashKind> {
	let config_path = common_dir.join("config");
	let common = common
		.try_clone()
		.map_err(|error| anyhow!("opening {}: {error}", common_dir.display()))?;
	let bytes = gitana_config_native::read_file_at(common, Path::new("config"), &config_path)
		.await?
		.ok_or_else(|| anyhow!("reading {}: file not found", config_path.display()))?;
	let text = std::str::from_utf8(&bytes)
		.map_err(|error| anyhow!("reading {}: {error}", config_path.display()))?;
	detect_algorithm_text(text)
}

fn detect_algorithm_text(text: &str) -> Result<HashKind> {
	let config = gitana_config::GitConfig::parse(text).map_err(|error| anyhow!("{error}"))?;
	match config
		.get_string("extensions", None, "objectformat")
		.unwrap_or("sha1")
	{
		"sha256" => Ok(HashKind::Sha256),
		"sha1" => Ok(HashKind::Sha1),
		other => bail!("unsupported object format: {other}"),
	}
}

/// A command that needs only the object graph and refs, written once over the repo's hash
/// algorithm `H`.
pub trait RepoCommand {
	/// Retain the effective command directory for operations that spawn configured child processes.
	fn set_command_directory(&mut self, _path: std::path::PathBuf, _directory: Dir) {}

	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()>;
}

/// A command that needs the working tree (index + work dir), also given the pathspec
/// `prefix` (the `/`-joined work-tree-relative subdirectory the command was invoked from).
pub trait WorkTreeCommand {
	/// Retain the effective command directory for operations that spawn configured child processes.
	fn set_command_directory(&mut self, _path: std::path::PathBuf, _directory: Dir) {}

	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()>;
}

#[derive(Clone, Copy)]
enum ConfigAccess {
	SetupOnly,
	Read,
	Mutation,
	HistoryMutation,
}

/// Discover the repository containing `cwd`, then run `command` under the repo's hash
/// algorithm. The single runtime→type bridge for object-graph commands.
pub async fn on_repo<C: RepoCommand>(cwd: &Path, command: C) -> Result<()> {
	on_repo_inner(cwd, command, ConfigAccess::SetupOnly).await
}

/// Discover the repository containing `cwd`, retaining shared-config serialization until the
/// command completes. Repository-local config readers use this path so deinit cannot temporarily
/// displace or replace the common config while it is being read.
pub async fn on_repo_config_read<C: RepoCommand>(cwd: &Path, command: C) -> Result<()> {
	on_repo_inner(cwd, command, ConfigAccess::Read).await
}

/// Discover the repository containing `cwd`, rejecting pending deinit recovery while retaining
/// shared-config serialization through the complete mutation.
pub async fn on_repo_config_mutation<C: RepoCommand>(cwd: &Path, command: C) -> Result<()> {
	on_repo_inner(cwd, command, ConfigAccess::Mutation).await
}

async fn on_repo_inner<C: RepoCommand>(cwd: &Path, command: C, access: ConfigAccess) -> Result<()> {
	let cwd = tokio::fs::canonicalize(cwd).await?;
	let found = repo::discover(&cwd).await?;
	let command_directory = RetainedCommandDirectory::capture(cwd).await?;
	let identity = repo::capture_repository_layout_identity(&found)?;
	on_discovered_repo(found, identity, command_directory, command, access).await
}

async fn on_discovered_repo<C: RepoCommand>(
	found: RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	command_directory: RetainedCommandDirectory,
	mut command: C,
	access: ConfigAccess,
) -> Result<()> {
	let (setup, common, git, worktree) = if matches!(access, ConfigAccess::Mutation) {
		repo::command_config_mutation_lease(&found, identity).await?
	} else {
		repo::command_setup_lease(&found, identity).await?
	};
	let repo::RevalidatedCommandDirectory {
		path: command_path,
		directory: cwd_directory,
		common,
		git,
		worktree: _,
	} = repo::revalidated_command_directory(&found, command_directory, &setup, common, git, worktree)
		.await?;
	command.set_command_directory(command_path, cwd_directory);
	if matches!(access, ConfigAccess::Mutation) {
		repo::ensure_no_pending_deinit_at(&found, &common, &git)?;
	}
	let retain_config_lease = !matches!(access, ConfigAccess::SetupOnly);
	match detect_algorithm_at(&common, &found.common_dir).await? {
		HashKind::Sha1 => {
			let repository =
				open_for_config_command::<Sha1>(&found, &setup, retain_config_lease, common, git).await?;
			run_with_config_lease(
				setup,
				retain_config_lease,
				Box::pin(command.run(repository)),
			)
			.await
		}
		HashKind::Sha256 => {
			let repository =
				open_for_config_command::<Sha256>(&found, &setup, retain_config_lease, common, git).await?;
			run_with_config_lease(
				setup,
				retain_config_lease,
				Box::pin(command.run(repository)),
			)
			.await
		}
	}
}

/// Discover the working tree containing `cwd`, then run `command` under the repo's hash
/// algorithm. The shared setup lease is retained through the complete command because ordinary
/// worktree operations can perform deferred raw configuration reads below their command boundary.
/// Errors in a bare repository (no work tree).
pub async fn on_worktree<C: WorkTreeCommand>(cwd: &Path, command: C) -> Result<()> {
	on_worktree_inner(cwd, command, ConfigAccess::Read).await
}

/// Discover the worktree containing `cwd`, retaining shared-config serialization until the command
/// completes. Worktree commands that read the common config use this path.
pub async fn on_worktree_config_read<C: WorkTreeCommand>(cwd: &Path, command: C) -> Result<()> {
	on_worktree_inner(cwd, command, ConfigAccess::Read).await
}

/// Discover the worktree containing `cwd`, rejecting pending deinit recovery while retaining
/// shared-config serialization through the complete mutation.
pub async fn on_worktree_config_mutation<C: WorkTreeCommand>(cwd: &Path, command: C) -> Result<()> {
	on_worktree_inner(cwd, command, ConfigAccess::Mutation).await
}

/// Run a multi-resource history operation under the worktree guard shared with submodule update.
/// Contention is reported before the command can alter its index, checkout, or operation state.
pub async fn on_worktree_history_mutation<C: WorkTreeCommand>(
	cwd: &Path,
	command: C,
) -> Result<()> {
	on_worktree_inner(cwd, command, ConfigAccess::HistoryMutation).await
}

async fn on_worktree_inner<C: WorkTreeCommand>(
	cwd: &Path,
	command: C,
	access: ConfigAccess,
) -> Result<()> {
	let (found, command_cwd, prefix) = repo::discover_worktree_with_prefix(cwd).await?;
	let command_directory = RetainedCommandDirectory::capture(command_cwd).await?;
	let identity = repo::capture_worktree_layout_identity(&found)?;
	on_discovered_worktree(found, identity, command_directory, prefix, command, access).await
}

async fn on_discovered_worktree<C: WorkTreeCommand>(
	found: RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	command_directory: RetainedCommandDirectory,
	prefix: String,
	mut command: C,
	access: ConfigAccess,
) -> Result<()> {
	let worktree_root = found.worktree_root.clone().expect("discovered work tree");
	let (history_guard, setup, common, git, work) = if matches!(access, ConfigAccess::HistoryMutation)
	{
		let (guard, setup, common, git, work) =
			repo::command_worktree_mutation_lease(&found, identity).await?;
		(Some(guard), setup, common, git, work)
	} else if matches!(access, ConfigAccess::Mutation) {
		let (setup, common, git, work) = repo::command_config_mutation_lease(&found, identity).await?;
		(None, setup, common, git, work)
	} else {
		let (setup, common, git, work) = repo::command_setup_lease(&found, identity).await?;
		(None, setup, common, git, work)
	};
	let repo::RevalidatedCommandDirectory {
		path: command_path,
		directory: cwd_directory,
		common,
		git,
		worktree: work,
	} = repo::revalidated_command_directory(&found, command_directory, &setup, common, git, work)
		.await?;
	let work = work.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;
	command.set_command_directory(command_path, cwd_directory);
	if matches!(access, ConfigAccess::Mutation) {
		repo::ensure_no_pending_deinit_at(&found, &common, &git)?;
	}
	let work = WorkDir::from_dir(work);
	let retain_config_lease = !matches!(access, ConfigAccess::SetupOnly);
	let result = async {
		match detect_algorithm_at(&common, &found.common_dir).await? {
			HashKind::Sha1 => {
				let wt = WorkTree::new_located(
					open_for_worktree_config_command::<Sha1>(
						&found,
						&setup,
						retain_config_lease,
						common,
						git,
					)
					.await?,
					work,
					found.git_dir,
					worktree_root,
				);
				run_with_config_lease(
					setup,
					retain_config_lease,
					Box::pin(command.run(wt, prefix)),
				)
				.await
			}
			HashKind::Sha256 => {
				let wt = WorkTree::new_located(
					open_for_worktree_config_command::<Sha256>(
						&found,
						&setup,
						retain_config_lease,
						common,
						git,
					)
					.await?,
					work,
					found.git_dir,
					worktree_root,
				);
				run_with_config_lease(
					setup,
					retain_config_lease,
					Box::pin(command.run(wt, prefix)),
				)
				.await
			}
		}
	}
	.await;
	if let Some(guard) = history_guard {
		guard.validate()?;
	}
	result
}

async fn run_with_config_lease(
	setup: gitana_submodule::SubmoduleMutationLease,
	retain: bool,
	operation: Pin<Box<dyn Future<Output = Result<()>> + '_>>,
) -> Result<()> {
	if !retain {
		drop(setup);
		return operation.await;
	}
	setup.validate()?;
	let result = operation.await;
	setup.validate()?;
	drop(setup);
	result
}

async fn open_for_config_command<H: HashAlgorithm>(
	found: &RepositoryLayout,
	setup: &gitana_submodule::SubmoduleMutationLease,
	retain_config_lease: bool,
	common: Dir,
	git: Dir,
) -> Result<Repository<Backend, H>> {
	if retain_config_lease {
		repo::open_generic_from_dirs_with_worker_lease::<H>(
			common,
			git,
			&found.git_dir,
			&found.common_dir,
			setup.clone(),
		)
		.await
	} else {
		repo::open_generic_from_dirs::<H>(common, git, &found.git_dir, &found.common_dir).await
	}
}

async fn open_for_worktree_config_command<H: HashAlgorithm>(
	found: &RepositoryLayout,
	setup: &gitana_submodule::SubmoduleMutationLease,
	retain_config_lease: bool,
	common: cap_std::fs::Dir,
	git: cap_std::fs::Dir,
) -> Result<Repository<Backend, H>> {
	if retain_config_lease {
		repo::open_generic_from_dirs_with_worker_lease::<H>(
			common,
			git,
			&found.git_dir,
			&found.common_dir,
			setup.clone(),
		)
		.await
	} else {
		repo::open_generic_from_dirs::<H>(common, git, &found.git_dir, &found.common_dir).await
	}
}

/// A command that operates on a single object named by a revision `spec`, written once
/// over the repo's hash algorithm. The dispatcher resolves the spec (including the
/// index-relative `:<path>` forms, which need the work tree) before handing over the id.
pub trait ObjectCommand {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<Backend, H>,
		oid: ObjectId<H>,
	) -> Result<()>;
}

/// Resolve `spec` to an object in the repository containing `cwd`, then run `command`
/// under the repo's hash algorithm.
pub async fn on_object<C: ObjectCommand>(cwd: &Path, spec: &str, command: C) -> Result<()> {
	let found = repo::discover(cwd).await?;
	let identity = repo::capture_repository_layout_identity(&found)?;
	let (setup, common, git, work) = repo::command_setup_lease(&found, identity).await?;
	match detect_algorithm_at(&common, &found.common_dir).await? {
		HashKind::Sha1 => {
			let (repo, oid) = resolve_object::<Sha1>(&found, spec, common, git, work).await?;
			drop(setup);
			command.run(repo, oid).await
		}
		HashKind::Sha256 => {
			let (repo, oid) = resolve_object::<Sha256>(&found, spec, common, git, work).await?;
			drop(setup);
			command.run(repo, oid).await
		}
	}
}

/// Resolve `spec` to `(repository, oid)` under `H`. An index-relative spec (`:<path>`)
/// opens the work tree (which holds the index); every other spec resolves against the
/// repository alone, so object-only lookups do not require a work tree.
async fn resolve_object<H: HashAlgorithm>(
	found: &RepositoryLayout,
	spec: &str,
	common: Dir,
	git: Dir,
	work: Option<Dir>,
) -> Result<(Repository<Backend, H>, ObjectId<H>)> {
	let resolve_common = common
		.try_clone()
		.map_err(|error| anyhow!("opening {}: {error}", found.common_dir.display()))?;
	let resolve_git = git
		.try_clone()
		.map_err(|error| anyhow!("opening {}: {error}", found.git_dir.display()))?;
	let repo =
		repo::open_generic_from_dirs::<H>(common, git, &found.git_dir, &found.common_dir).await?;
	let oid = if spec.starts_with(':') {
		let worktree_root = found
			.worktree_root
			.clone()
			.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;
		let directory =
			WorkDir::from_dir(work.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?);
		WorkTree::new_located(
			repo::open_generic_from_dirs::<H>(
				resolve_common,
				resolve_git,
				&found.git_dir,
				&found.common_dir,
			)
			.await?,
			directory,
			found.git_dir.clone(),
			worktree_root,
		)
		.rev_parse(spec)
		.await?
	} else {
		repo.rev_parse(spec).await?
	};
	Ok((repo, oid))
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
	use std::fs::{OpenOptions, TryLockError};

	use cap_std::{ambient_authority, fs::Dir};
	use gitana_submodule::acquire_submodule_config_mutation_lease;

	use super::run_with_config_lease;

	#[tokio::test]
	async fn config_lease_runner_retains_or_releases_serialization_as_requested() {
		let temporary = tempfile::tempdir().unwrap();
		let common_path = temporary.path();
		std::fs::create_dir(common_path.join("refs")).unwrap();
		let common = Dir::open_ambient_dir(common_path, ambient_authority()).unwrap();
		let lock_path = common_path.join("gitana-submodule-config.lock");

		let retained = acquire_submodule_config_mutation_lease(&common, common_path).unwrap();
		run_with_config_lease(
			retained,
			true,
			Box::pin(async {
				let competing = OpenOptions::new()
					.read(true)
					.write(true)
					.open(&lock_path)
					.unwrap();
				assert!(matches!(
					competing.try_lock(),
					Err(TryLockError::WouldBlock)
				));
				Ok(())
			}),
		)
		.await
		.unwrap();

		let released = acquire_submodule_config_mutation_lease(&common, common_path).unwrap();
		run_with_config_lease(
			released,
			false,
			Box::pin(async {
				let competing = OpenOptions::new()
					.read(true)
					.write(true)
					.open(&lock_path)
					.unwrap();
				competing.try_lock().unwrap();
				Ok(())
			}),
		)
		.await
		.unwrap();
	}
}

#[cfg(all(test, unix))]
mod worktree_tests {
	use std::fs::{OpenOptions, TryLockError};
	#[cfg(target_os = "linux")]
	use std::os::unix::ffi::OsStringExt as _;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Arc, Mutex};

	use anyhow::Result;
	use cap_std::{ambient_authority, fs::Dir};
	use gitana_object::{HashAlgorithm, HashKind, Sha1};
	use gitana_repository::Repository;
	use gitana_submodule::{
		acquire_submodule_config_mutation_lease, acquire_worktree_mutation_guard,
	};
	use gitana_worktree::WorkTree;

	use super::{
		ConfigAccess, RepoCommand, WorkTreeCommand, detect_algorithm_at, on_discovered_repo,
		on_discovered_worktree, on_worktree,
	};
	use crate::repo;
	use crate::{Backend, RetainedCommandDirectory, WorkDir};

	struct PausedWorktreeCommand {
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
	}

	struct RetainedCwdCommand {
		directory: Option<Dir>,
		entered: Arc<AtomicBool>,
		release: Arc<AtomicBool>,
		observed: Arc<Mutex<Option<String>>>,
	}

	impl WorkTreeCommand for RetainedCwdCommand {
		fn set_command_directory(&mut self, _path: std::path::PathBuf, directory: Dir) {
			self.directory = Some(directory);
		}

		async fn run<H: HashAlgorithm>(
			self,
			_worktree: WorkTree<Backend, WorkDir, H>,
			_prefix: String,
		) -> Result<()> {
			self.entered.store(true, Ordering::SeqCst);
			while !self.release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
			let value = self
				.directory
				.expect("dispatch retained the command directory")
				.read_to_string("signer-context")?;
			*self.observed.lock().unwrap() = Some(value);
			Ok(())
		}
	}

	impl WorkTreeCommand for PausedWorktreeCommand {
		async fn run<H: HashAlgorithm>(
			self,
			_worktree: WorkTree<Backend, WorkDir, H>,
			_prefix: String,
		) -> Result<()> {
			self.entered.store(true, Ordering::SeqCst);
			while !self.release.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
			Ok(())
		}
	}

	struct RecordingRepoCommand {
		entered: Arc<AtomicBool>,
	}

	impl RepoCommand for RecordingRepoCommand {
		async fn run<H: HashAlgorithm>(self, _repo: Repository<Backend, H>) -> Result<()> {
			self.entered.store(true, Ordering::SeqCst);
			Ok(())
		}
	}

	#[tokio::test]
	async fn repository_dispatch_rejects_a_repository_replaced_while_waiting() {
		let temporary = tempfile::tempdir().unwrap();
		let visible = temporary.path().join("repository.git");
		let retained = temporary.path().join("repository-retained.git");
		std::fs::create_dir_all(visible.join("objects")).unwrap();
		std::fs::create_dir_all(visible.join("refs")).unwrap();
		std::fs::write(visible.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			visible.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
		)
		.unwrap();
		std::fs::create_dir_all(retained.join("objects")).unwrap();
		std::fs::create_dir_all(retained.join("refs")).unwrap();
		std::fs::write(retained.join("HEAD"), "ref: refs/heads/replacement\n").unwrap();
		std::fs::write(
			retained.join("config"),
			"[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n",
		)
		.unwrap();
		let parent = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let visible_name = visible.file_name().unwrap();
		let retained_name = retained.file_name().unwrap();
		let visible_entry = gitana_fs_native::entry_identity(&parent, visible_name).unwrap();
		let retained_entry = gitana_fs_native::entry_identity(&parent, retained_name).unwrap();
		let found = repo::inspect_root(&visible).await.unwrap();
		let identity = repo::capture_repository_layout_identity(&found).unwrap();
		let common = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &visible).unwrap();
		let entered = Arc::new(AtomicBool::new(false));

		let operation = on_discovered_repo(
			found,
			identity,
			RetainedCommandDirectory::capture(visible.clone())
				.await
				.unwrap(),
			RecordingRepoCommand {
				entered: Arc::clone(&entered),
			},
			ConfigAccess::Read,
		);
		let replace = async {
			tokio::task::yield_now().await;
			gitana_fs_native::replace_if_identities(
				&parent,
				retained_name,
				retained_entry,
				visible_name,
				visible_entry,
			)
			.unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(operation, replace);

		let error = result.expect_err("dispatch must reject the replacement repository");
		assert!(
			error
				.to_string()
				.contains("repository changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
		assert!(!entered.load(Ordering::SeqCst));
		assert_eq!(
			std::fs::read_to_string(visible.join("config")).unwrap(),
			"[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n"
		);
	}

	#[tokio::test]
	async fn ordinary_worktree_dispatch_retains_setup_serialization() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		let objects = git_dir.join("objects");
		let refs = git_dir.join("refs");
		std::fs::create_dir_all(&objects).unwrap();
		std::fs::create_dir(&refs).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let operation = on_worktree(
			&worktree,
			PausedWorktreeCommand {
				entered: Arc::clone(&entered),
				release: Arc::clone(&release),
			},
		);
		let inspect = async {
			for _ in 0..100_000 {
				if entered.load(Ordering::SeqCst) {
					break;
				}
				tokio::task::yield_now().await;
			}
			assert!(entered.load(Ordering::SeqCst));

			let competing = OpenOptions::new().read(true).open(&refs).unwrap();
			assert!(matches!(
				competing.try_lock(),
				Err(TryLockError::WouldBlock)
			));
			release.store(true, Ordering::SeqCst);
			competing
		};
		let (result, competing) = tokio::join!(operation, inspect);
		result.unwrap();
		competing.try_lock().unwrap();
	}

	#[tokio::test]
	async fn worktree_dispatch_rejects_a_nested_command_directory_replaced_while_waiting() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let command_directory = worktree.join("nested");
		let retired = worktree.join("nested-retired");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(&command_directory).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		let found = repo::inspect_root(&worktree).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&found).unwrap();
		let retained =
			RetainedCommandDirectory::capture(std::fs::canonicalize(&command_directory).unwrap())
				.await
				.unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();
		let entered = Arc::new(AtomicBool::new(false));

		let operation = on_discovered_worktree(
			found,
			identity,
			retained,
			"nested".to_owned(),
			PausedWorktreeCommand {
				entered: Arc::clone(&entered),
				release: Arc::new(AtomicBool::new(true)),
			},
			ConfigAccess::Read,
		);
		let replace = async {
			tokio::task::yield_now().await;
			std::fs::rename(&command_directory, &retired).unwrap();
			std::fs::create_dir(&command_directory).unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(operation, replace);

		let error = result.expect_err("dispatch must reject a replaced command directory");
		assert!(
			error
				.to_string()
				.contains("command working directory changed while waiting"),
			"unexpected error: {error:#}"
		);
		assert!(!entered.load(Ordering::SeqCst));
	}

	#[tokio::test]
	async fn history_dispatch_contends_before_entering_the_command() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		let git = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let retained = acquire_worktree_mutation_guard(&git, &git_dir).unwrap();
		let entered = Arc::new(AtomicBool::new(false));

		let result = super::on_worktree_history_mutation(
			&worktree,
			PausedWorktreeCommand {
				entered: Arc::clone(&entered),
				release: Arc::new(AtomicBool::new(true)),
			},
		)
		.await;

		let error = result.expect_err("a second history mutation must contend");
		assert!(
			error
				.to_string()
				.contains("another Gitana worktree mutation is in progress"),
			"unexpected error: {error:#}"
		);
		assert!(!entered.load(Ordering::SeqCst));
		drop(retained);
	}

	#[tokio::test]
	async fn ordinary_worktree_dispatch_retains_the_original_command_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let retained = temporary.path().join("retained");
		let command_directory = worktree.join("nested");
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(&command_directory).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(command_directory.join("signer-context"), "original").unwrap();

		let entered = Arc::new(AtomicBool::new(false));
		let release = Arc::new(AtomicBool::new(false));
		let observed = Arc::new(Mutex::new(None));
		let operation = on_worktree(
			&command_directory,
			RetainedCwdCommand {
				directory: None,
				entered: Arc::clone(&entered),
				release: Arc::clone(&release),
				observed: Arc::clone(&observed),
			},
		);
		let replace = async {
			while !entered.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
			std::fs::rename(&worktree, &retained).unwrap();
			std::fs::create_dir_all(&command_directory).unwrap();
			std::fs::write(command_directory.join("signer-context"), "replacement").unwrap();
			release.store(true, Ordering::SeqCst);
		};
		let (result, ()) = tokio::join!(operation, replace);
		result.unwrap();
		assert_eq!(observed.lock().unwrap().as_deref(), Some("original"));
	}

	#[cfg(target_os = "linux")]
	#[tokio::test]
	async fn ordinary_worktree_dispatch_preserves_a_non_utf8_command_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let command_directory = worktree.join(std::ffi::OsString::from_vec(b"nested-\xff".to_vec()));
		let git_dir = worktree.join(".git");
		std::fs::create_dir_all(&command_directory).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(command_directory.join("signer-context"), "native").unwrap();

		let observed = Arc::new(Mutex::new(None));
		on_worktree(
			&command_directory,
			RetainedCwdCommand {
				directory: None,
				entered: Arc::new(AtomicBool::new(false)),
				release: Arc::new(AtomicBool::new(true)),
				observed: Arc::clone(&observed),
			},
		)
		.await
		.unwrap();

		assert_eq!(observed.lock().unwrap().as_deref(), Some("native"));
	}

	#[tokio::test]
	async fn ordinary_worktree_dispatch_rejects_a_checkout_retired_while_waiting() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let retained = temporary.path().join("retained");
		let git_dir = temporary.path().join("module.git");
		std::fs::create_dir_all(&worktree).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(worktree.join(".git"), "gitdir: ../module.git\n").unwrap();
		let found = repo::inspect_root(&worktree).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&found).unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();
		let entered = Arc::new(AtomicBool::new(false));

		let operation = on_discovered_worktree(
			found,
			identity,
			RetainedCommandDirectory::capture(worktree.clone())
				.await
				.unwrap(),
			String::new(),
			PausedWorktreeCommand {
				entered: Arc::clone(&entered),
				release: Arc::new(AtomicBool::new(true)),
			},
			ConfigAccess::Read,
		);
		let retire = async {
			tokio::task::yield_now().await;
			std::fs::rename(&worktree, &retained).unwrap();
			std::fs::create_dir(&worktree).unwrap();
			std::fs::write(worktree.join(".git"), "gitdir: ../module.git\n").unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(operation, retire);

		let error = result.expect_err("a retired worktree must not reach its command");
		assert!(
			error
				.to_string()
				.contains("worktree changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
		assert!(!entered.load(Ordering::SeqCst));
		assert_eq!(
			std::fs::read(worktree.join(".git")).unwrap(),
			b"gitdir: ../module.git\n"
		);
		assert!(retained.join(".git").is_file());
	}

	#[tokio::test]
	async fn revalidated_capabilities_supply_hash_and_config_after_gitdir_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = temporary.path().join("module.git");
		let retained = temporary.path().join("module-retained.git");
		std::fs::create_dir(&worktree).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n[user]\n\tname = Original\n",
		)
		.unwrap();
		std::fs::write(
			git_dir.join("config.worktree"),
			"[gitana-test]\n\torigin = original\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(worktree.join(".git"), "gitdir: ../module.git\n").unwrap();

		let found = repo::inspect_root(&worktree).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&found).unwrap();
		let (setup, common, git, _work) = repo::command_setup_lease(&found, identity).await.unwrap();

		std::fs::rename(&git_dir, &retained).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 1\n\tbare = false\n[extensions]\n\tobjectformat = sha256\n\tworktreeConfig = true\n[user]\n\tname = Replacement\n",
		)
		.unwrap();
		std::fs::write(
			git_dir.join("config.worktree"),
			"[gitana-test]\n\torigin = replacement\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/replacement\n").unwrap();

		assert_eq!(
			detect_algorithm_at(&common, &found.common_dir)
				.await
				.unwrap(),
			HashKind::Sha1
		);
		let repository =
			repo::open_generic_from_dirs::<Sha1>(common, git, &found.git_dir, &found.common_dir)
				.await
				.unwrap();
		let effective = repository.effective_config().await.unwrap();
		assert_eq!(effective.get_string("user", None, "name"), Some("Original"));
		assert_eq!(
			effective.get_string("gitana-test", None, "origin"),
			Some("original")
		);
		setup.validate().unwrap();
	}

	#[tokio::test]
	async fn revalidated_capabilities_retain_pending_recovery_after_gitdir_replacement() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("work");
		let git_dir = temporary.path().join("module.git");
		let retained = temporary.path().join("module-retained.git");
		std::fs::create_dir(&worktree).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
		std::fs::create_dir(git_dir.join("gitana-submodule-deinit")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(worktree.join(".git"), "gitdir: ../module.git\n").unwrap();

		let found = repo::inspect_root(&worktree).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&found).unwrap();
		let (setup, common, git, _work) = repo::command_setup_lease(&found, identity).await.unwrap();

		std::fs::rename(&git_dir, &retained).unwrap();
		std::fs::create_dir_all(git_dir.join("objects")).unwrap();
		std::fs::create_dir_all(git_dir.join("refs")).unwrap();
		std::fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/replacement\n").unwrap();

		let error = repo::ensure_no_pending_deinit_at(&found, &common, &git)
			.expect_err("the retained recovery owner must remain authoritative");
		assert!(
			error.to_string().contains("pending submodule deinit"),
			"unexpected error: {error:#}"
		);
		setup.validate().unwrap();
	}
}
