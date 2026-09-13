//! Local repository construction over the reusable [`gitana_repository_layout`] discovery API.
//!
//! Discovery itself — walking up to a repository, resolving `.git` files, `commondir`, and bare
//! repositories to a canonical [`RepositoryLayout`] — lives in `gitana-repository-layout` and is re-exported
//! here. This module keeps the gta-specific pieces: minting filesystem capabilities from the
//! discovered paths, installing git's effective config, and the worktree-checkout guards.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cap_fs_ext::DirExt as _;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_submodule::{
	SubmoduleError, SubmoduleMutationLease, WorktreeMutationGuard,
	acquire_submodule_config_mutation_lease, acquire_submodule_config_setup_lease,
	acquire_worktree_mutation_guard, deinit_recovery_git_dirs,
	pending_deinit_configs_require_restore, pending_set_url_configs_require_restore,
	repository_has_pending_deinit, repository_has_pending_set_url,
	repository_has_set_url_participant_claim, restore_pending_deinit_configs,
	restore_pending_set_url_configs, try_acquire_submodule_config_setup_lease,
};

use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
use gitana_fs_native::{EntryIdentity, directory_identity};
use gitana_repository_layout::{
	discover as discover_layout_impl, try_discover as try_discover_layout_impl,
};

use crate::submodule_configuration::WorktreeConfiguration;
use crate::{Backend, RepositoryLayoutIdentity, RetainedCommandDirectory, WorkDir};

pub use gitana_repository_layout::{DiscoveryError, RepositoryLayout, inspect_root};

const SETUP_RECOVERY_BACKOFF_INITIAL: Duration = Duration::from_millis(10);
const SETUP_RECOVERY_BACKOFF_MAX: Duration = Duration::from_millis(250);

/// Discover the containing repository without performing command-setup recovery.
///
/// This is reserved for callers that must distinguish a genuine absence before deciding whether a
/// repository command will run. Actual repository operations must use [`discover`].
pub(crate) async fn discover_layout(start: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	discover_layout_impl(start).await
}

/// Discover the containing repository for an ordinary command.
///
/// Callers that read repository configuration must retain [`command_setup_lease`] through those
/// reads so a concurrent Windows deinit cannot expose its temporary config displacement.
pub async fn discover(start: &Path) -> Result<RepositoryLayout> {
	Ok(discover_layout_impl(start).await?)
}

/// Like [`discover`], while preserving the outside-repository `None` result.
pub async fn try_discover(start: &Path) -> Result<Option<RepositoryLayout>> {
	let Some(layout) = try_discover_layout_impl(start).await? else {
		return Ok(None);
	};
	Ok(Some(layout))
}

/// Retain submodule config serialization while a command reads its hash kind and effective config.
///
/// Lock acquisition waits in a blocking worker. If a crashed Windows deinit left the shared config
/// displaced, this drops the read lease, restores the exact journaled before-image, and reacquires
/// serialization before returning.
pub(crate) async fn command_setup_lease(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
) -> Result<(SubmoduleMutationLease, Dir, Dir, Option<Dir>)> {
	let mut recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL;
	loop {
		let lease = acquire_setup_lease_once(layout).await?;
		let (common, git, worktree) = revalidate_repository_layout(layout, expected).await?;
		if !pending_configs_require_restore_at(layout, &common, &git, worktree.as_ref()).await? {
			lease.validate()?;
			return Ok((lease, common, git, worktree));
		}
		drop(lease);
		match restore_command_setup(layout, common, git, worktree).await {
			Ok(()) => recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL,
			Err(error) if matches!(error.downcast_ref(), Some(SubmoduleError::UpdateLocked)) => {
				backoff_after_setup_recovery_lock(&mut recovery_backoff).await;
			}
			Err(error) => return Err(error),
		}
	}
}

/// Serialize a history operation before it can mutate this worktree's index or checkout.
///
/// The first setup pass repairs any recoverable Windows config displacement without holding the
/// per-worktree guard that recovery itself needs. The guard is then acquired before the second
/// shared-config setup lease, matching submodule update's per-worktree-then-common lock order.
pub(crate) async fn command_worktree_mutation_lease(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
) -> Result<(
	WorktreeMutationGuard,
	SubmoduleMutationLease,
	Dir,
	Dir,
	Option<Dir>,
)> {
	command_worktree_mutation_lease_impl(layout, expected, false).await
}

async fn command_worktree_mutation_lease_impl(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
	mut force_retry_after_guard: bool,
) -> Result<(
	WorktreeMutationGuard,
	SubmoduleMutationLease,
	Dir,
	Dir,
	Option<Dir>,
)> {
	loop {
		let (initial_setup, _, _, _) = command_setup_lease(layout, expected).await?;
		drop(initial_setup);

		let (_, lock_git, _) = revalidate_repository_layout(layout, expected).await?;
		let git_dir = layout.git_dir.clone();
		let guard =
			tokio::task::spawn_blocking(move || acquire_worktree_mutation_guard(&lock_git, &git_dir))
				.await
				.map_err(|error| anyhow!("acquiring worktree mutation lock: {error}"))?
				.map_err(|error| match error {
					SubmoduleError::UpdateLocked => {
						anyhow!("another Gitana worktree mutation is in progress")
					}
					other => anyhow::Error::from(other),
				})?;

		// Do not call the recovery-aware setup loop while holding `guard`: config recovery acquires
		// this same per-worktree lock. Inspect once under setup serialization and retry from the
		// guard-free phase if a crash window appeared after the initial pass.
		let setup = acquire_setup_lease_once(layout).await?;
		let (common, git, worktree) = revalidate_repository_layout(layout, expected).await?;
		let forced_retry = std::mem::take(&mut force_retry_after_guard);
		if forced_retry
			|| pending_configs_require_restore_at(layout, &common, &git, worktree.as_ref()).await?
		{
			drop(setup);
			drop(guard);
			continue;
		}
		setup.validate()?;
		guard.validate()?;
		let combined = guard.lease().combine(setup);
		return Ok((guard, combined, common, git, worktree));
	}
}

/// Retain shared-config serialization for read-only linked-worktree enumeration.
///
/// Listing deliberately omits symlinked administrative namespaces. When the resolved shared-config
/// target is present it therefore must not require the stricter recovery-owner enumeration used by
/// writers. A missing resolved target can be a recoverable Windows publication gap, so that state
/// still takes the normal recovery-aware setup path.
pub(crate) async fn worktree_list_setup_lease(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
) -> Result<(SubmoduleMutationLease, Dir, Dir, Option<Dir>)> {
	let lease = acquire_setup_lease_once(layout).await?;
	let (common, git, worktree) = revalidate_repository_layout(layout, expected).await?;
	let config_path = layout.common_dir.join("config");
	let config = gitana_config_native::read_file_at(
		common
			.try_clone()
			.with_context(|| format!("opening {}", layout.common_dir.display()))?,
		Path::new("config"),
		&config_path,
	)
	.await?;
	if config.is_some() {
		lease.validate()?;
		return Ok((lease, common, git, worktree));
	}
	drop(lease);
	command_setup_lease(layout, expected).await
}

/// Serialize exact-root local-source config reads without assuming recovery authority.
///
/// Clone and transfer sources are inspected as independent repositories and never restore or
/// advance a deinit transaction owned by that source. Standalone readers wait for a live config
/// publisher. A transfer already retaining an unrelated mutation lease tries without waiting so
/// reciprocal local sources cannot form a cross-repository lock cycle.
pub(crate) async fn local_source_setup_lease(
	layout: &RepositoryLayout,
	held: Option<&SubmoduleMutationLease>,
) -> Result<SubmoduleMutationLease> {
	let common = Dir::open_ambient_dir(&layout.common_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?;
	if let Some(held) = held
		&& held
			.covers_config_directory(&common)
			.map_err(|error| anyhow!("identifying {}: {error}", layout.common_dir.display()))?
	{
		held.validate()?;
		return Ok(held.clone());
	}
	if held.is_some() {
		try_acquire_setup_lease_once(layout).await
	} else {
		acquire_setup_lease_once(layout).await
	}
}

/// Wait for source serialization, then bind all subsequent reads to the repository inspected
/// before the wait.
pub(crate) async fn revalidated_local_source_setup(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
	held: Option<&SubmoduleMutationLease>,
) -> Result<(SubmoduleMutationLease, Dir, Dir)> {
	let lease = local_source_setup_lease(layout, held).await?;
	let (common, git, _) = revalidate_repository_layout(layout, expected).await?;
	lease.validate()?;
	Ok((lease, common, git))
}

async fn acquire_setup_lease_once(layout: &RepositoryLayout) -> Result<SubmoduleMutationLease> {
	let common_dir = layout.common_dir.clone();
	let lease = tokio::task::spawn_blocking(move || {
		let common = Dir::open_ambient_dir(&common_dir, ambient_authority()).map_err(|source| {
			SubmoduleError::Open {
				path: common_dir.clone(),
				source,
			}
		})?;
		acquire_submodule_config_setup_lease(&common, &common_dir)
	})
	.await
	.map_err(|error| anyhow!("waiting for repository command setup lock: {error}"))?
	.map_err(anyhow::Error::from)?;
	lease.validate()?;
	Ok(lease)
}

async fn try_acquire_setup_lease_once(layout: &RepositoryLayout) -> Result<SubmoduleMutationLease> {
	let common_dir = layout.common_dir.clone();
	let lease = tokio::task::spawn_blocking(move || {
		let common = Dir::open_ambient_dir(&common_dir, ambient_authority()).map_err(|source| {
			SubmoduleError::Open {
				path: common_dir.clone(),
				source,
			}
		})?;
		try_acquire_submodule_config_setup_lease(&common, &common_dir)
	})
	.await
	.map_err(|error| anyhow!("trying repository command setup lock: {error}"))?
	.map_err(anyhow::Error::from)?;
	lease.validate()?;
	Ok(lease)
}

/// Retain exclusive shared-config serialization through a frontend-owned configuration mutation.
///
/// This uses the same missing-config recovery loop as command setup, but takes the mutation form of
/// the stable guard and the repository-owned named lock before returning.
pub(crate) async fn command_config_mutation_lease(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
) -> Result<(SubmoduleMutationLease, Dir, Dir, Option<Dir>)> {
	let mut recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL;
	loop {
		let (lock_common, _, _) = revalidate_repository_layout(layout, expected).await?;
		let common_dir = layout.common_dir.clone();
		let lease = tokio::task::spawn_blocking(move || {
			acquire_submodule_config_mutation_lease(&lock_common, &common_dir)
		})
		.await
		.map_err(|error| anyhow!("waiting for repository config mutation lock: {error}"))??;

		let (common, git, worktree) = revalidate_repository_layout(layout, expected).await?;
		if !pending_configs_require_restore_at(layout, &common, &git, worktree.as_ref()).await? {
			lease.validate()?;
			return Ok((lease, common, git, worktree));
		}
		drop(lease);
		match restore_command_setup(layout, common, git, worktree).await {
			Ok(()) => recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL,
			Err(error) if matches!(error.downcast_ref(), Some(SubmoduleError::UpdateLocked)) => {
				backoff_after_setup_recovery_lock(&mut recovery_backoff).await;
			}
			Err(error) => return Err(error),
		}
	}
}

async fn backoff_after_setup_recovery_lock(delay: &mut Duration) {
	tokio::time::sleep(*delay).await;
	*delay = delay.saturating_mul(2).min(SETUP_RECOVERY_BACKOFF_MAX);
}

fn pending_submodule_command_at(
	layout: &RepositoryLayout,
	common: &Dir,
	git: &Dir,
) -> Result<Option<&'static str>> {
	if repository_has_pending_deinit(common, git, layout)? {
		return Ok(Some("deinit"));
	}
	for (git_dir, owner) in deinit_recovery_git_dirs(common, git, layout)? {
		if repository_has_pending_set_url(&owner, &git_dir)? {
			return Ok(Some("set-url"));
		}
	}
	Ok(None)
}

async fn pending_configs_require_restore_at(
	layout: &RepositoryLayout,
	common: &Dir,
	git: &Dir,
	worktree: Option<&Dir>,
) -> Result<bool> {
	let current_identity =
		directory_identity(git).with_context(|| format!("identifying {}", layout.git_dir.display()))?;
	for (git_dir, git) in deinit_recovery_git_dirs(common, git, layout)? {
		let configuration = WorktreeConfiguration::new(
			common
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?,
			git
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?,
			&layout.common_dir,
			&git_dir,
		);
		if pending_deinit_configs_require_restore(git.try_clone()?, &git_dir, &configuration).await? {
			return Ok(true);
		}
		let owner_identity =
			directory_identity(&git).with_context(|| format!("identifying {}", git_dir.display()))?;
		let work = if owner_identity == current_identity {
			worktree
				.map(|work| {
					work.try_clone().map(|work| {
						(
							work,
							layout
								.worktree_root
								.clone()
								.expect("work capability has a root"),
						)
					})
				})
				.transpose()?
		} else {
			None
		};
		if pending_set_url_configs_require_restore(
			git
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?,
			&git_dir,
			common.try_clone()?,
			&layout.common_dir,
			work,
			&configuration,
		)
		.await?
		{
			return Ok(true);
		}
		if repository_has_set_url_participant_claim(&git, &git_dir)?
			&& gitana_config_native::read_file_at(
				git.try_clone()?,
				Path::new("config"),
				&git_dir.join("config"),
			)
			.await?
			.is_none()
		{
			return Err(SubmoduleError::RecoveryRequired(format!(
				"a parent submodule set-url left '{}' temporarily unavailable; retry that set-url from its owning superproject",
				git_dir.join("config").display()
			))
			.into());
		}
	}
	Ok(false)
}

/// Reject pending recovery through repository directories already bound to a checked layout.
///
/// The caller must retain the corresponding setup or mutation lease while checking and throughout
/// the operation so the result remains atomic with deinit intent publication.
pub(crate) fn ensure_no_pending_deinit_at(
	layout: &RepositoryLayout,
	common: &Dir,
	git: &Dir,
) -> Result<()> {
	if let Some(command) = pending_submodule_command_at(layout, common, git)? {
		return pending_submodule_recovery_error(command);
	}
	Ok(())
}

/// Reject deinit recovery while allowing the current command to resume set-URL recovery.
pub(crate) fn ensure_no_deinit_recovery_at(
	layout: &RepositoryLayout,
	common: &Dir,
	git: &Dir,
) -> Result<()> {
	if repository_has_pending_deinit(common, git, layout)? {
		return pending_submodule_recovery_error("deinit");
	}
	Ok(())
}

fn pending_submodule_recovery_error(command: &str) -> Result<()> {
	let message = if command == "set-url" {
		"a pending submodule set-url must be retried from its owning superproject before changing repository configuration".to_owned()
	} else {
		format!(
			"a pending submodule {command} must be completed with 'gta submodule {command}' before changing repository configuration"
		)
	};
	Err(SubmoduleError::RecoveryRequired(message).into())
}

async fn restore_command_setup(
	layout: &RepositoryLayout,
	common: Dir,
	git: Dir,
	worktree: Option<Dir>,
) -> Result<()> {
	let current_identity = directory_identity(&git)
		.with_context(|| format!("identifying {}", layout.git_dir.display()))?;
	for (git_dir, git) in deinit_recovery_git_dirs(&common, &git, layout)? {
		let recovery_git = git
			.try_clone()
			.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?;
		let configuration = WorktreeConfiguration::new(
			common
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?,
			git
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?,
			&layout.common_dir,
			&git_dir,
		);
		restore_pending_deinit_configs(
			recovery_git,
			&git_dir,
			common
				.try_clone()
				.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?,
			&layout.common_dir,
			&configuration,
		)
		.await?;
		let owner_identity =
			directory_identity(&git).with_context(|| format!("identifying {}", git_dir.display()))?;
		let work = if owner_identity == current_identity {
			worktree
				.as_ref()
				.map(|work| {
					work.try_clone().map(|work| {
						(
							work,
							layout
								.worktree_root
								.clone()
								.expect("work capability has a root"),
						)
					})
				})
				.transpose()?
		} else {
			None
		};
		restore_pending_set_url_configs(
			git.try_clone()?,
			&git_dir,
			common.try_clone()?,
			&layout.common_dir,
			work,
			&configuration,
		)
		.await?;
	}
	Ok(())
}

/// The stable local-transport URL selected by exact-root repository inspection.
///
/// [`RepositoryLayout`] paths are already canonical and absolute. Deriving the URL from the layout
/// avoids resolving the caller's ambient spelling a second time after source capabilities have been
/// retained, when a symlink in that spelling could already point at a different repository.
pub(crate) fn local_source_url(layout: &RepositoryLayout) -> Result<String> {
	let root = layout.worktree_root.as_deref().unwrap_or(&layout.git_dir);
	root
		.to_str()
		.map(ToOwned::to_owned)
		.ok_or_else(|| anyhow!("local remote path is not valid UTF-8: {}", root.display()))
}

/// Capture the filesystem identities behind a discovered repository layout before waiting.
pub(crate) fn capture_repository_layout_identity(
	layout: &RepositoryLayout,
) -> Result<RepositoryLayoutIdentity> {
	let worktree = layout
		.worktree_root
		.as_deref()
		.map(|path| open_identity_directory(path, "work tree"))
		.transpose()?;
	let git = open_identity_directory(&layout.git_dir, "worktree Git directory")?;
	let common = open_identity_directory(&layout.common_dir, "common Git directory")?;
	Ok(RepositoryLayoutIdentity {
		worktree: worktree
			.as_ref()
			.map(directory_identity)
			.transpose()
			.with_context(|| {
				format!(
					"identifying work tree {}",
					layout
						.worktree_root
						.as_deref()
						.expect("opened work tree")
						.display()
				)
			})?,
		git: directory_identity(&git)
			.with_context(|| format!("identifying Git directory {}", layout.git_dir.display()))?,
		common: directory_identity(&common).with_context(|| {
			format!(
				"identifying common Git directory {}",
				layout.common_dir.display()
			)
		})?,
	})
}

/// Capture a discovered worktree, rejecting bare repositories.
pub(crate) fn capture_worktree_layout_identity(
	layout: &RepositoryLayout,
) -> Result<RepositoryLayoutIdentity> {
	if layout.worktree_root.is_none() {
		return Err(work_tree_required());
	}
	capture_repository_layout_identity(layout)
}

/// Open the command's effective working directory through the repository capabilities that were
/// revalidated after setup serialization was acquired.
///
/// `cwd` is the canonical directory used for discovery. Resolving its suffix below a retained root
/// keeps later child-process setup bound to the same repository even if the public root is renamed
/// and replaced. Every component is opened without following a new symlink.
fn retained_command_directory(
	layout: &RepositoryLayout,
	cwd: &Path,
	common: &Dir,
	git: &Dir,
	worktree: Option<&Dir>,
) -> Result<Dir> {
	if let (Some(root), Some(directory)) = (layout.worktree_root.as_deref(), worktree)
		&& let Ok(relative) = cwd.strip_prefix(root)
	{
		return open_retained_subdirectory(directory, relative, cwd);
	}
	if let Ok(relative) = cwd.strip_prefix(&layout.git_dir) {
		return open_retained_subdirectory(git, relative, cwd);
	}
	if let Ok(relative) = cwd.strip_prefix(&layout.common_dir) {
		return open_retained_subdirectory(common, relative, cwd);
	}
	bail!(
		"command working directory is outside the retained repository: {}",
		cwd.display()
	)
}

/// Revalidate the visible command directory after setup waiting, then return the capability opened
/// before that wait. Later relative-resource reads therefore remain bound to the invocation's exact
/// directory even if its public name is replaced after this boundary.
pub(crate) struct RevalidatedCommandDirectory {
	pub(crate) path: PathBuf,
	pub(crate) directory: Dir,
	pub(crate) common: Dir,
	pub(crate) git: Dir,
	pub(crate) worktree: Option<Dir>,
}

pub(crate) async fn revalidated_command_directory(
	layout: &RepositoryLayout,
	retained: RetainedCommandDirectory,
	setup: &SubmoduleMutationLease,
	common: Dir,
	git: Dir,
	worktree: Option<Dir>,
) -> Result<RevalidatedCommandDirectory> {
	let layout = layout.clone();
	let setup = setup.clone();
	let (path, directory, identity) = retained.into_parts();
	tokio::task::spawn_blocking(move || {
		let visible = retained_command_directory(&layout, &path, &common, &git, worktree.as_ref())?;
		validate_retained_command_directory(&path, &directory, identity, &visible)?;
		setup.validate()?;
		Ok(RevalidatedCommandDirectory {
			path,
			directory,
			common,
			git,
			worktree,
		})
	})
	.await
	.context("joining command-directory revalidation worker")?
}

fn validate_retained_command_directory(
	path: &Path,
	retained: &Dir,
	identity: EntryIdentity,
	visible: &Dir,
) -> Result<()> {
	let retained = directory_identity(retained).with_context(|| {
		format!(
			"identifying retained command working directory {}",
			path.display()
		)
	})?;
	let current = directory_identity(visible).with_context(|| {
		format!(
			"identifying visible command working directory {}",
			path.display()
		)
	})?;
	if retained != identity || current != identity {
		bail!(
			"command working directory changed while waiting for repository setup: {}",
			path.display()
		);
	}
	Ok(())
}

/// Open a canonical repository-relative command directory without following replacement symlinks.
fn open_retained_subdirectory(root: &Dir, relative: &Path, display: &Path) -> Result<Dir> {
	let mut directory = root
		.try_clone()
		.with_context(|| format!("retaining command working directory {}", display.display()))?;
	for component in relative.components() {
		let std::path::Component::Normal(name) = component else {
			if matches!(component, std::path::Component::CurDir) {
				continue;
			}
			bail!("invalid command working directory: {}", display.display());
		};
		directory = directory
			.open_dir_nofollow(name)
			.with_context(|| format!("retaining command working directory {}", display.display()))?;
	}
	Ok(directory)
}

fn open_identity_directory(path: &Path, kind: &str) -> Result<Dir> {
	Dir::open_ambient_dir(path, ambient_authority())
		.with_context(|| format!("opening {kind} {}", path.display()))
}

/// Verify that a serialized command still names the worktree discovered before it waited.
///
/// The caller must retain the repository's setup or mutation lease. Exact-root inspection rejects
/// an empty replacement or a checkout rebound to another repository, while the inode comparison
/// also rejects a replacement that recreates the same marker and administrative paths. The
/// returned capabilities are the exact directories whose identities were checked.
pub(crate) async fn revalidate_repository_layout(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
) -> Result<(Dir, Dir, Option<Dir>)> {
	let root = layout
		.worktree_root
		.as_deref()
		.unwrap_or(&layout.common_dir);
	let subject = if layout.worktree_root.is_some() {
		"worktree"
	} else {
		"repository"
	};
	let current = inspect_root(root).await.with_context(|| {
		format!(
			"{subject} changed while waiting for repository setup: {}",
			root.display()
		)
	})?;
	if &current != layout {
		bail!(
			"{subject} changed while waiting for repository setup: {}",
			root.display()
		);
	}
	let worktree = layout
		.worktree_root
		.as_deref()
		.map(|path| open_identity_directory(path, "work tree"))
		.transpose()?;
	let git = open_identity_directory(&layout.git_dir, "worktree Git directory")?;
	let common = open_identity_directory(&layout.common_dir, "common Git directory")?;
	let actual = RepositoryLayoutIdentity {
		worktree: worktree
			.as_ref()
			.map(directory_identity)
			.transpose()
			.with_context(|| format!("identifying work tree {}", root.display()))?,
		git: directory_identity(&git)
			.with_context(|| format!("identifying Git directory {}", layout.git_dir.display()))?,
		common: directory_identity(&common).with_context(|| {
			format!(
				"identifying common Git directory {}",
				layout.common_dir.display()
			)
		})?,
	};
	if actual != expected {
		bail!(
			"{subject} changed while waiting for repository setup: {}",
			root.display()
		);
	}
	Ok((common, git, worktree))
}

/// The error for a work-tree operation run in a bare repo (or outside a work tree).
fn work_tree_required() -> anyhow::Error {
	anyhow!("this operation must be run in a work tree")
}

/// Open the repository whose per-worktree files live under `git_dir` and whose shared files live
/// under `common_dir`, under an explicit hash algorithm `H`. (The two are the same path for an
/// ordinary, non-linked repository.) The runtime dispatch (see [`crate::dispatch`]) picks `H` from
/// the repo's config and calls this, so each command body is monomorphised once per algorithm.
///
/// This is the program edge — the one place gta mints ambient filesystem authority from a path — so
/// it also assembles git's effective (merged) configuration here ([`crate::git_config`] needs the
/// same ambient path access) and installs it on the repository. The engine then honours a
/// global/system `remote.*` / `pack.packSizeLimit` / `core.logallrefupdates`, matching git; a
/// malformed global/system file aborts the command, as git does. A repo whose local `config` does
/// not exist yet (a fresh `init`/`clone`) still resolves the global and system layers.
pub async fn open_generic<H: HashAlgorithm>(
	git_dir: &Path,
	common_dir: &Path,
) -> Result<Repository<Backend, H>> {
	open_generic_inner(git_dir, common_dir, None).await
}

/// Open a repository from directory capabilities already rebound to a discovered layout.
pub(crate) async fn open_generic_from_dirs<H: HashAlgorithm>(
	common: Dir,
	git: Dir,
	git_dir: &Path,
	common_dir: &Path,
) -> Result<Repository<Backend, H>> {
	open_generic_from_dirs_inner(common, git, git_dir, common_dir, None).await
}

/// Open a capability-pinned repository whose detached workers retain config serialization.
pub(crate) async fn open_generic_from_dirs_with_worker_lease<H: HashAlgorithm>(
	common: Dir,
	git: Dir,
	git_dir: &Path,
	common_dir: &Path,
	lease: SubmoduleMutationLease,
) -> Result<Repository<Backend, H>> {
	let keepalive: Arc<dyn Send + Sync> = Arc::new(lease);
	open_generic_from_dirs_inner(common, git, git_dir, common_dir, Some(keepalive)).await
}

async fn open_generic_inner<H: HashAlgorithm>(
	git_dir: &Path,
	common_dir: &Path,
	worker_keepalive: Option<Arc<dyn Send + Sync>>,
) -> Result<Repository<Backend, H>> {
	// The store is capability-pure: open the (already-created) directories here, at the
	// program edge, and hand the capabilities in.
	let common = Dir::open_ambient_dir(common_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", common_dir.display()))?;
	let git = Dir::open_ambient_dir(git_dir, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?;
	open_generic_from_dirs_inner(common, git, git_dir, common_dir, worker_keepalive).await
}

async fn open_generic_from_dirs_inner<H: HashAlgorithm>(
	common: Dir,
	git: Dir,
	git_dir: &Path,
	common_dir: &Path,
	worker_keepalive: Option<Arc<dyn Send + Sync>>,
) -> Result<Repository<Backend, H>> {
	let config_common = common
		.try_clone()
		.map_err(|error| anyhow!("opening {}: {error}", common_dir.display()))?;
	let config_git = git
		.try_clone()
		.map_err(|error| anyhow!("opening {}: {error}", git_dir.display()))?;
	let effective =
		crate::git_config::for_worktree_at(config_common, config_git, common_dir, git_dir).await?;
	let store = match worker_keepalive {
		Some(keepalive) => WorktreeFileStore::new_with_worker_keepalive(common, git, keepalive),
		None => WorktreeFileStore::new(common, git),
	};
	let mut repo = Repository::new(ObjectStore::new(store));
	// The effective config includes this worktree's `config.worktree` layer when
	// `extensions.worktreeConfig` is set, matching git's precedence (system < global < local <
	// config.worktree). `for_worktree_at` degrades to the common config when the extension is off or
	// the file is absent, and reads both repository-owned layers through the retained capabilities.
	repo.set_effective_config(effective);
	Ok(repo)
}

/// Open the working tree at `work` as a filesystem capability. Like [`open_generic`], this is a
/// program-edge point that mints ambient authority from a path — the working-tree counterpart to the
/// git-directory capability the store holds.
pub fn open_work_dir(work: &Path) -> Result<WorkDir> {
	let dir = Dir::open_ambient_dir(work, ambient_authority())
		.map_err(|error| anyhow!("opening work tree {}: {error}", work.display()))?;
	Ok(CapWorkDir::from_dir(dir))
}

/// Discover the working tree containing `start` as a [`RepositoryLayout`], the canonical native
/// command directory, and the pathspec `prefix`, without constructing a typed `WorkTree`. The
/// runtime dispatch needs the paths so it can build a `WorkTree<_, H>` for whichever hash algorithm
/// the repo uses. The prefix is the `/`-joined path from the work-tree root down to `start` (empty at
/// the root), making pathspecs relative to the caller's subdirectory, the way `git -C <subdir>` does.
/// The native path must remain separate because the pathspec string can be lossy on Unix.
pub async fn discover_worktree_with_prefix(
	start: &Path,
) -> Result<(RepositoryLayout, PathBuf, String)> {
	// Resolve symlinks before discovering, so the prefix reflects the physical location of the
	// caller's directory under the work tree (e.g. `-C linksub` where `linksub -> sub`).
	// Otherwise the lexical name would be matched/recorded as a tracked path. Discovery canonicalizes
	// internally too, so `worktree_root` is a canonical ancestor of this canonical `start` — the strip
	// below stays purely lexical.
	let start = std::fs::canonicalize(start)?;
	let found = discover(&start).await?;
	let work = found
		.worktree_root
		.as_ref()
		.ok_or_else(work_tree_required)?;
	// `work` is the canonical `start` with trailing components removed, so this strip succeeds.
	let prefix = start
		.strip_prefix(work)
		.unwrap_or(Path::new(""))
		.components()
		.map(|component| component.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/");
	Ok((found, start, prefix))
}

/// If `branch` (a full ref like `refs/heads/main`) is checked out in a *different* worktree of this
/// repository, return that worktree's working directory. git forbids checking out one branch in two
/// worktrees at once: the branch ref is shared, so the two checkouts would race when committing.
///
/// `git_dir` is the current checkout's per-worktree git directory, excluded from the scan (switching
/// to the branch this worktree is already on is not a conflict). Errors if `git_dir`'s `commondir` is
/// corrupt (rather than silently treating the repository as self-contained).
pub async fn branch_checked_out_elsewhere(git_dir: &Path, branch: &str) -> Result<Option<PathBuf>> {
	let common_dir = gitana_repository_layout::common_dir_of(git_dir).await?;
	Ok(branch_checkout_location(&common_dir, branch, Some(git_dir)))
}

/// The working directory of a worktree (the main one, or a linked one under `common_dir`) whose
/// `HEAD` is the symbolic ref `branch`, skipping `exclude` (a git directory to ignore — typically the
/// caller's own). git shares a branch ref across a repository's worktrees, so a branch may be checked
/// out in at most one at a time; this locates that one.
///
/// Unlike [`branch_checked_out_elsewhere`], `common_dir` is passed directly, so this works before the
/// caller's own git directory exists (as `worktree add` needs, checking a not-yet-created worktree).
pub(crate) fn branch_checkout_location(
	common_dir: &Path,
	branch: &str,
	exclude: Option<&Path>,
) -> Option<PathBuf> {
	let exclude = exclude.map(canonical);
	worktree_git_dirs(common_dir, is_bare(common_dir))
		.into_iter()
		.find(|candidate| {
			exclude.as_ref() != Some(&canonical(candidate))
				&& head_symbolic_target(candidate).as_deref() == Some(branch)
		})
		.map(|candidate| worktree_path_of(&candidate))
}

/// Every branch checked out in a worktree of this repository — the main one and each linked one —
/// paired with that worktree's working directory. This is the set a plain `fetch` (or `pull`) must
/// refuse to update: git shares a branch ref across a repository's worktrees, so fetching directly into
/// a branch a worktree has checked out would desync that checkout's index/work tree from its ref.
///
/// The *current* worktree is included; the fetch guard tells it apart from the others by `HEAD` (a
/// `pull` may still advance the current branch via its merge step, whereas any other checked-out branch
/// is refused outright). Detached / unborn worktrees contribute nothing. `bare` is the caller's
/// serialized repository-config snapshot so this scan never rereads a temporarily displaced config.
pub(crate) fn branch_checkouts(common_dir: &Path, bare: bool) -> Vec<(String, PathBuf)> {
	worktree_git_dirs(common_dir, bare)
		.into_iter()
		.filter_map(|candidate| {
			head_symbolic_target(&candidate).map(|branch| (branch, worktree_path_of(&candidate)))
		})
		.collect()
}

/// Capture repository-owned `core.bare` from the symlink-aware effective config installed when the
/// repository was opened. System, global, and command-scope values cannot redefine repository
/// identity, while `config.worktree` remains part of the repository-owned stack when enabled.
pub(crate) async fn repository_bare_snapshot<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
) -> Result<bool> {
	Ok(
		repository
			.effective_config()
			.await?
			.get_repository_bool("core", None, "bare")?
			.unwrap_or(false),
	)
}

/// Every worktree's git directory for the repository at `common_dir`: the main worktree (`common_dir`
/// itself, unless the repo is bare, where its HEAD is not a checkout) and each
/// `<common_dir>/worktrees/<name>`.
fn worktree_git_dirs(common_dir: &Path, bare: bool) -> Vec<PathBuf> {
	let mut git_dirs = Vec::new();
	if !bare {
		git_dirs.push(common_dir.to_path_buf());
	}
	if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
		for entry in entries.flatten() {
			if entry.path().join("HEAD").is_file() {
				git_dirs.push(entry.path());
			}
		}
	}
	git_dirs
}

/// The symbolic ref `<git_dir>/HEAD` points at (e.g. `refs/heads/main`), or `None` when HEAD is
/// detached (a raw object id) or unreadable.
fn head_symbolic_target(git_dir: &Path) -> Option<String> {
	std::fs::read_to_string(git_dir.join("HEAD"))
		.ok()
		.and_then(|head| {
			head
				.strip_prefix("ref:")
				.map(|target| target.trim().to_owned())
		})
}

/// The working directory for a worktree named by its git directory, matching git's own resolution:
///
/// - A **linked** worktree's admin directory carries a `gitdir` backlink to the worktree's `.git`
///   file; its parent is the working directory (git's `get_linked_worktree`).
/// - The **main** worktree is the common directory with a trailing `/.git` stripped (git's
///   `get_main_worktree`): an ordinary `<work>/.git` yields `<work>`, while a git directory detached
///   from its work tree — `--separate-git-dir` or a symlinked `.git`, canonicalized by discovery —
///   yields the git directory itself. git resolves this from the common directory alone, ignoring the
///   real working tree and `core.worktree`, so the result is the same from any worktree.
pub(crate) fn worktree_path_of(git_dir: &Path) -> PathBuf {
	if let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) {
		// `gitdir` points at the worktree's `.git` file; its parent is the working directory. git may
		// write a relative pointer (`worktree.useRelativePaths` / `--relative-paths`), resolved against
		// the admin directory — not the caller's cwd.
		let pointer = Path::new(text.trim());
		let git_file = if pointer.is_absolute() {
			pointer.to_path_buf()
		} else {
			git_dir.join(pointer)
		};
		if let Some(parent) = git_file.parent() {
			return parent.to_path_buf();
		}
	}
	// The main worktree: strip a trailing `.git` component from the common (git) directory.
	if git_dir.file_name() == Some(OsStr::new(".git"))
		&& let Some(parent) = git_dir.parent()
	{
		return parent.to_path_buf();
	}
	git_dir.to_path_buf()
}

/// Whether the repository at `common_dir` is bare (`core.bare`), so it has no main working tree and
/// its `HEAD` is not a checkout. Uses git's boolean grammar (`true`/`yes`/`on`/`1`/valueless), not a
/// literal `"true"` match.
pub(crate) fn is_bare(common_dir: &Path) -> bool {
	std::fs::read_to_string(common_dir.join("config"))
		.ok()
		.and_then(|text| gitana_config::GitConfig::parse(&text).ok())
		.and_then(|config| config.get_bool("core", None, "bare").ok().flatten())
		.unwrap_or(false)
}

fn canonical(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(all(test, any(unix, windows)))]
mod worktree_mutation_tests {
	use std::time::Duration;

	use super::*;

	#[tokio::test]
	async fn mutation_setup_drops_its_guard_before_a_recovery_retry() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("worktree");
		let git = worktree.join(".git");
		std::fs::create_dir_all(git.join("objects")).unwrap();
		std::fs::create_dir_all(git.join("refs")).unwrap();
		std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			git.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		let layout = inspect_root(&worktree).await.unwrap();
		let identity = capture_worktree_layout_identity(&layout).unwrap();

		let acquired = tokio::time::timeout(
			Duration::from_secs(1),
			command_worktree_mutation_lease_impl(&layout, identity, true),
		)
		.await
		.expect("a recovery retry must not contend with its own worktree guard")
		.unwrap();
		drop(acquired);
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::os::unix::fs::symlink;

	use super::*;

	#[tokio::test(start_paused = true)]
	async fn setup_recovery_lock_backoff_grows_and_caps() {
		let mut delay = SETUP_RECOVERY_BACKOFF_INITIAL;
		let mut observed = Vec::new();
		for _ in 0..7 {
			let started = tokio::time::Instant::now();
			backoff_after_setup_recovery_lock(&mut delay).await;
			observed.push(tokio::time::Instant::now() - started);
		}

		assert_eq!(
			observed,
			[
				Duration::from_millis(10),
				Duration::from_millis(20),
				Duration::from_millis(40),
				Duration::from_millis(80),
				Duration::from_millis(160),
				Duration::from_millis(250),
				Duration::from_millis(250),
			]
		);
		assert_eq!(delay, SETUP_RECOVERY_BACKOFF_MAX);
	}

	#[test]
	fn branch_checkouts_uses_the_callers_bare_snapshot_during_a_missing_config_window() {
		let temporary = tempfile::tempdir().unwrap();
		let common = temporary.path().join("common.git");
		let linked = temporary.path().join("linked");
		let linked_git = common.join("worktrees/linked");
		std::fs::create_dir_all(&linked_git).unwrap();
		std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(linked_git.join("HEAD"), "ref: refs/heads/dev\n").unwrap();
		std::fs::write(
			linked_git.join("gitdir"),
			format!("{}\n", linked.join(".git").display()),
		)
		.unwrap();

		assert_eq!(
			branch_checkouts(&common, true),
			vec![("refs/heads/dev".to_owned(), linked)]
		);
	}

	fn create_bare_repository(path: &Path) {
		std::fs::create_dir_all(path.join("objects")).unwrap();
		std::fs::create_dir_all(path.join("refs")).unwrap();
		std::fs::write(path.join("HEAD"), "ref: refs/heads/main\n").unwrap();
	}

	#[tokio::test]
	async fn config_mutation_rejects_a_replacement_before_creating_its_lock() {
		let temporary = tempfile::tempdir().unwrap();
		let visible = temporary.path().join("repository.git");
		let replacement = temporary.path().join("replacement.git");
		create_bare_repository(&visible);
		create_bare_repository(&replacement);
		let layout = inspect_root(&visible).await.unwrap();
		let identity = capture_repository_layout_identity(&layout).unwrap();
		let parent = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let visible_name = visible.file_name().unwrap();
		let replacement_name = replacement.file_name().unwrap();
		let visible_identity = gitana_fs_native::entry_identity(&parent, visible_name).unwrap();
		let replacement_identity = gitana_fs_native::entry_identity(&parent, replacement_name).unwrap();
		gitana_fs_native::replace_if_identities(
			&parent,
			replacement_name,
			replacement_identity,
			visible_name,
			visible_identity,
		)
		.unwrap();

		let error = match command_config_mutation_lease(&layout, identity).await {
			Ok(_) => panic!("config mutation must reject the replacement repository"),
			Err(error) => error,
		};
		assert!(
			error
				.to_string()
				.contains("repository changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
		assert!(
			!visible.join("gitana-submodule-config.lock").exists(),
			"validation must precede creation of the replacement repository's mutation lock"
		);
	}

	#[tokio::test]
	async fn local_source_url_stays_bound_to_the_inspected_layout() {
		let temporary = tempfile::tempdir().unwrap();
		let source_a = temporary.path().join("source-a");
		let source_b = temporary.path().join("source-b");
		let alias = temporary.path().join("source");
		create_bare_repository(&source_a);
		create_bare_repository(&source_b);
		symlink(&source_a, &alias).unwrap();

		let layout = inspect_root(&alias).await.unwrap();
		std::fs::remove_file(&alias).unwrap();
		symlink(&source_b, &alias).unwrap();

		assert_eq!(
			local_source_url(&layout).unwrap(),
			std::fs::canonicalize(&source_a).unwrap().to_str().unwrap()
		);
		assert_ne!(
			local_source_url(&layout).unwrap(),
			std::fs::canonicalize(&alias).unwrap().to_str().unwrap()
		);
	}

	#[tokio::test]
	async fn standalone_local_source_setup_waits_without_bootstrapping_recovery() {
		use std::sync::mpsc::{RecvTimeoutError, channel};
		use std::time::Duration;

		let temporary = tempfile::tempdir().unwrap();
		let source = temporary.path().join("source.git");
		create_bare_repository(&source);
		let layout = inspect_root(&source).await.unwrap();
		let common = Dir::open_ambient_dir(&layout.common_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &layout.common_dir).unwrap();
		let control = layout.git_dir.join("gitana-submodule-deinit");
		std::fs::create_dir(&control).unwrap();
		std::fs::write(control.join("intent.json"), b"not a recoverable intent").unwrap();

		let (started_sender, started_receiver) = channel();
		let (acquired_sender, acquired_receiver) = channel();
		let waiter = std::thread::spawn(move || {
			let runtime = tokio::runtime::Builder::new_current_thread()
				.build()
				.unwrap();
			started_sender.send(()).unwrap();
			let lease = runtime
				.block_on(local_source_setup_lease(&layout, None))
				.unwrap();
			acquired_sender.send(()).unwrap();
			lease
		});
		started_receiver.recv().unwrap();
		assert!(matches!(
			acquired_receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Timeout)
		));

		drop(mutation);
		acquired_receiver
			.recv_timeout(Duration::from_secs(1))
			.expect("source config reads wait for a live mutation");
		drop(waiter.join().unwrap());
		assert!(
			!source.join("config").exists(),
			"source serialization must not recover or synthesize config"
		);
		assert_eq!(
			std::fs::read(control.join("intent.json")).unwrap(),
			b"not a recoverable intent"
		);
	}

	#[tokio::test]
	async fn local_source_setup_rejects_a_repository_replaced_while_waiting() {
		let temporary = tempfile::tempdir().unwrap();
		let source = temporary.path().join("source.git");
		let retained = temporary.path().join("source-retained.git");
		create_bare_repository(&source);
		std::fs::write(
			source.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
		)
		.unwrap();
		create_bare_repository(&retained);
		std::fs::write(
			retained.join("config"),
			"[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n",
		)
		.unwrap();
		let parent = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let source_name = source.file_name().unwrap();
		let retained_name = retained.file_name().unwrap();
		let source_entry = gitana_fs_native::entry_identity(&parent, source_name).unwrap();
		let retained_entry = gitana_fs_native::entry_identity(&parent, retained_name).unwrap();
		let layout = inspect_root(&source).await.unwrap();
		let identity = capture_repository_layout_identity(&layout).unwrap();
		let common = Dir::open_ambient_dir(&layout.common_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &layout.common_dir).unwrap();

		let operation = revalidated_local_source_setup(&layout, identity, None);
		let replace = async {
			tokio::task::yield_now().await;
			gitana_fs_native::replace_if_identities(
				&parent,
				retained_name,
				retained_entry,
				source_name,
				source_entry,
			)
			.unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(operation, replace);

		let error = match result {
			Ok(_) => panic!("a replacement source must be rejected after the wait"),
			Err(error) => error,
		};
		assert!(
			error
				.to_string()
				.contains("repository changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
		assert_eq!(
			std::fs::read_to_string(source.join("config")).unwrap(),
			"[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n"
		);
	}

	#[tokio::test]
	async fn reciprocal_local_sources_fail_fast_instead_of_forming_a_lock_cycle() {
		let temporary = tempfile::tempdir().unwrap();
		let first_source = temporary.path().join("first.git");
		let second_source = temporary.path().join("second.git");
		create_bare_repository(&first_source);
		create_bare_repository(&second_source);
		let first_layout = inspect_root(&first_source).await.unwrap();
		let second_layout = inspect_root(&second_source).await.unwrap();
		let first_common =
			Dir::open_ambient_dir(&first_layout.common_dir, ambient_authority()).unwrap();
		let second_common =
			Dir::open_ambient_dir(&second_layout.common_dir, ambient_authority()).unwrap();
		let first_mutation =
			acquire_submodule_config_mutation_lease(&first_common, &first_layout.common_dir).unwrap();
		let second_mutation =
			acquire_submodule_config_mutation_lease(&second_common, &second_layout.common_dir).unwrap();

		let error = match local_source_setup_lease(&second_layout, Some(&first_mutation)).await {
			Ok(_) => panic!("an unrelated held lease must make source contention fail fast"),
			Err(error) => error,
		};
		assert!(matches!(
			error.downcast_ref::<SubmoduleError>(),
			Some(SubmoduleError::UpdateLocked)
		));
		let error = match local_source_setup_lease(&first_layout, Some(&second_mutation)).await {
			Ok(_) => panic!("reciprocal source contention must not wait"),
			Err(error) => error,
		};
		assert!(matches!(
			error.downcast_ref::<SubmoduleError>(),
			Some(SubmoduleError::UpdateLocked)
		));
		drop(second_mutation);
		drop(first_mutation);
	}

	#[tokio::test]
	async fn local_source_setup_reuses_a_held_lock_for_the_same_repository() {
		let temporary = tempfile::tempdir().unwrap();
		let source = temporary.path().join("source.git");
		create_bare_repository(&source);
		let layout = inspect_root(&source).await.unwrap();
		let common = Dir::open_ambient_dir(&layout.common_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &layout.common_dir).unwrap();

		let reused = local_source_setup_lease(&layout, Some(&mutation))
			.await
			.unwrap();
		assert!(reused.covers_config_directory(&common).unwrap());
		drop(reused);
		drop(mutation);
	}
}
