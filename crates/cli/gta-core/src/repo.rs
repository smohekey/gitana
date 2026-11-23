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
use gitana_path::GitPath;
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

pub use gitana_repository_layout::{
	DiscoveryError, RepositoryLayout, inspect_root, inspect_source,
};

const SETUP_RECOVERY_BACKOFF_INITIAL: Duration = Duration::from_millis(10);
const SETUP_RECOVERY_BACKOFF_MAX: Duration = Duration::from_millis(250);

/// Discover the containing repository without performing command-setup recovery.
///
/// This is reserved for callers that must distinguish a genuine absence before deciding whether a
/// repository command will run. Actual repository operations must use [`discover`].
pub(crate) async fn discover_layout(start: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	discover_layout_impl(start).await
}

/// `found` looked at again while holding its setup lock, the one command setup waits on
/// ([`command_setup_lease`]). Discovery reads the config for the work tree (`core.worktree`,
/// `core.bare`), and a publisher holding the lock may have that config moved aside (a Windows
/// submodule deinit), so the first look may have read the gap; this waits it out. The same place is
/// inspected again, as command setup's revalidation inspects it, and taken only if it's still the
/// same repository (otherwise, or with no lock to take, what was found stands). The repository's
/// identities are taken before the wait and checked after it, so one put in its place meanwhile is
/// refused, as command setup refuses it, not taken as the baseline its callers then record.
///
/// `start`, the folder the command started in, is checked across the wait too: callers record it
/// only once this returns, so one put in its place meanwhile would otherwise be their baseline. And
/// a crashed deinit's config left moved aside is restored first, as command setup restores it, so
/// the config read is the repository's own, not the gap.
async fn settled(start: &Path, found: RepositoryLayout) -> Result<RepositoryLayout> {
	let root = found
		.marker_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());
	let Ok(before) = capture_repository_layout_identity(&found) else {
		// Not identifiable now: the caller's own capture reports why.
		return Ok(found);
	};
	let started = command_directory_identity(start).ok();
	let mut recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL;
	let (layout, lease) = loop {
		let Ok(lease) = acquire_setup_lease_once(&found).await else {
			break (found.clone(), None);
		};
		// The recovery is best-effort here: a probe or restore that fails leaves things as they were,
		// and command setup, which runs the same recovery, reports it for a command that needs it (a
		// command that doesn't, as `worktree list`, isn't stopped by it).
		if let Ok((common, git, worktree)) =
			revalidate_layout_as(&found, before, Inspection::Root, Compared::Identities).await
			&& pending_configs_require_restore_at(&found, &common, &git, worktree.as_ref())
				.await
				.unwrap_or(false)
		{
			drop(lease);
			match restore_command_setup(&found, common, git, worktree).await {
				Ok(()) => {
					recovery_backoff = SETUP_RECOVERY_BACKOFF_INITIAL;
					continue;
				}
				Err(error) if matches!(error.downcast_ref(), Some(SubmoduleError::UpdateLocked)) => {
					backoff_after_setup_recovery_lock(&mut recovery_backoff).await;
					continue;
				}
				Err(_) => break (found.clone(), None),
			}
		}
		let again = inspect_root(&root).await.ok();
		let layout = match again {
			Some(again) if again.git_dir == found.git_dir && again.common_dir == found.common_dir => {
				again
			}
			_ => found.clone(),
		};
		break (layout, Some(lease));
	};
	let after = capture_repository_layout_identity(&layout);
	let started_after = started.map(|_| command_directory_identity(start));
	drop(lease);
	let subject = if layout.worktree_root.is_some() {
		"worktree"
	} else {
		"repository"
	};
	let changed = || {
		anyhow!(
			"{subject} changed while waiting for repository setup: {}",
			root.display()
		)
	};
	let after = after.map_err(|_| changed())?;
	if after.git != before.git
		|| after.common != before.common
		|| (layout.worktree_root == found.worktree_root && after.worktree != before.worktree)
	{
		return Err(changed());
	}
	if let (Some(started), Some(now)) = (started, started_after)
		&& now.ok() != Some(started)
	{
		bail!(
			"command working directory changed while waiting for repository setup: {}",
			start.display()
		);
	}
	Ok(layout)
}

/// The identity of the folder at `path`, a command's starting folder.
fn command_directory_identity(path: &Path) -> Result<EntryIdentity> {
	let directory = open_identity_directory(path, "command working directory")?;
	directory_identity(&directory)
		.with_context(|| format!("identifying command working directory {}", path.display()))
}

/// The folder a command that needs no work tree (fetch, push, `trust sync`) runs from, started in
/// `cwd`, as git's runs (probed vs git 2.55): the work tree's top when started inside it, otherwise
/// the folder it was started in (where `.git` is with the work tree elsewhere, inside the git folder,
/// a bare repository or a folder in it). A relative remote URL, askpass or `GIT_SSH_COMMAND` resolves
/// from here. (`pull` needs a work tree, so git runs it from the top.)
pub(crate) async fn repository_command_directory(
	found: &RepositoryLayout,
	cwd: &Path,
) -> Result<PathBuf> {
	let started = tokio::fs::canonicalize(cwd).await?;
	Ok(match found.worktree_root.as_deref() {
		Some(top) if started.starts_with(top) => top.to_path_buf(),
		_ => started,
	})
}

/// git's warning for a repository whose config sets both `core.bare` and `core.worktree` (it takes
/// it as bare), given on stderr as git gives it, once a command for each repository however often
/// the command discovers it (probed vs git 2.55).
fn warn_of_its_config(layout: &RepositoryLayout) {
	static WARNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
	if !layout.bare_and_worktree {
		return;
	}
	let mut warned = WARNED
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner);
	if warned.contains(&layout.git_dir) {
		return;
	}
	warned.push(layout.git_dir.clone());
	eprintln!("warning: core.bare and core.worktree do not make sense");
}

/// How a repository was inspected, so it's inspected the same way when checked again.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inspection {
	/// As a repository root ([`inspect_root`]): its work tree too.
	Root,
	/// As a local clone's source ([`inspect_source`]): its git folders only, as git's local clone
	/// reads one (a work tree its config names needn't exist).
	Source,
}

/// Inspect exactly `root` as `inspection` says.
pub(crate) async fn inspect_as(
	root: &Path,
	inspection: Inspection,
) -> Result<RepositoryLayout, DiscoveryError> {
	match inspection {
		Inspection::Root => inspect_root(root).await,
		Inspection::Source => inspect_source(root).await,
	}
}

/// How a local source given as `url` is read: a plain path as git's local clone reads it, without
/// its work tree; a `file://` URL as through git's upload-pack, which needs the work tree (probed vs
/// git 2.55: a plain-path clone works from a source whose `core.worktree` is missing, a `file://`
/// one fails).
pub(crate) fn local_clone_inspection(url: &str) -> Inspection {
	if url.starts_with("file://") {
		Inspection::Root
	} else {
		Inspection::Source
	}
}

/// Discover the containing repository for an ordinary command.
///
/// Callers that read repository configuration must retain [`command_setup_lease`] through those
/// reads so a concurrent Windows deinit cannot expose its temporary config displacement.
pub async fn discover(start: &Path) -> Result<RepositoryLayout> {
	let layout = settled(start, discover_layout_impl(start).await?).await?;
	warn_of_its_config(&layout);
	Ok(layout)
}

/// Like [`discover`], while preserving the outside-repository `None` result.
pub async fn try_discover(start: &Path) -> Result<Option<RepositoryLayout>> {
	let Some(found) = try_discover_layout_impl(start).await? else {
		return Ok(None);
	};
	let layout = settled(start, found).await?;
	warn_of_its_config(&layout);
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
/// `inspection` is how `layout` was inspected.
pub(crate) async fn revalidated_local_source_setup(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
	held: Option<&SubmoduleMutationLease>,
	inspection: Inspection,
) -> Result<(SubmoduleMutationLease, Dir, Dir)> {
	let lease = local_source_setup_lease(layout, held).await?;
	let (common, git, _) =
		revalidate_layout_as(layout, expected, inspection, Compared::ReadFrom).await?;
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
	// The repository as it's found again (where its `.git` is, or a bare git directory), not its
	// work tree, which `core.worktree` can put somewhere with no `.git`.
	let root = layout.marker_root.as_deref().unwrap_or(&layout.git_dir);
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

/// Whether `cwd` is under the folder holding `.git` but outside the work tree, the git directory and
/// the common directory: a repository whose `core.worktree` names another folder, run from where its
/// `.git` is.
fn outside_work_tree(layout: &RepositoryLayout, cwd: &Path) -> bool {
	layout
		.marker_root
		.as_deref()
		.is_some_and(|marker| cwd.starts_with(marker))
		&& !layout
			.worktree_root
			.as_deref()
			.is_some_and(|root| cwd.starts_with(root))
		&& !cwd.starts_with(&layout.git_dir)
		&& !cwd.starts_with(&layout.common_dir)
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

/// Whether a command needs a work tree, which decides where it runs when started outside one.
#[derive(Clone, Copy)]
pub(crate) enum CommandNeeds {
	/// A work tree (add, status, pull, …): started outside it, it runs from the work tree's top.
	WorkTree,
	/// Only the repository (tag, trust, push, …): it runs where it was started.
	Repository,
}

pub(crate) async fn revalidated_command_directory(
	layout: &RepositoryLayout,
	retained: RetainedCommandDirectory,
	setup: &SubmoduleMutationLease,
	common: Dir,
	git: Dir,
	worktree: Option<Dir>,
	needs: CommandNeeds,
) -> Result<RevalidatedCommandDirectory> {
	let layout = layout.clone();
	let setup = setup.clone();
	let (path, directory, identity) = retained.into_parts();
	tokio::task::spawn_blocking(move || {
		let in_work_tree = layout
			.worktree_root
			.as_deref()
			.is_some_and(|root| path.starts_with(root));
		// A command that needs a work tree, started outside it (where `.git` is, or inside the git
		// folder, `core.worktree` putting the work tree elsewhere): git changes into the work tree's
		// top for it (probed vs git 2.55), so that is its directory; the folder it was started in is
		// only checked unchanged.
		if matches!(needs, CommandNeeds::WorkTree)
			&& !in_work_tree
			&& let (Some(root), Some(top)) = (layout.worktree_root.clone(), worktree.as_ref())
		{
			validate_visible_command_directory(&path, &directory, identity)?;
			setup.validate()?;
			let top = top
				.try_clone()
				.with_context(|| format!("opening the work tree for {}", path.display()))?;
			return Ok(RevalidatedCommandDirectory {
				path: root,
				directory: top,
				common,
				git,
				worktree,
			});
		}
		// Started in the folder holding `.git`, outside the work tree and the git folders, any other
		// command stays where it was started, as git's does (a relative signing key is found there):
		// that folder, checked unchanged against its name opened again.
		if outside_work_tree(&layout, &path) {
			validate_visible_command_directory(&path, &directory, identity)?;
			setup.validate()?;
			return Ok(RevalidatedCommandDirectory {
				path,
				directory,
				common,
				git,
				worktree,
			});
		}
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

/// The folder a command was started in, outside the repository's folders, checked unchanged: its
/// name opened again must still be the folder retained when it started.
fn validate_visible_command_directory(
	path: &Path,
	retained: &Dir,
	identity: EntryIdentity,
) -> Result<()> {
	let visible = open_identity_directory(path, "command working directory")?;
	validate_retained_command_directory(path, retained, identity, &visible)
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
	revalidate_layout_as(layout, expected, Inspection::Root, Compared::Whole).await
}

/// What of a layout found again must match the one found first.
#[derive(Clone, Copy)]
enum Compared {
	/// All of it: a command works in its work tree too.
	Whole,
	/// What a local source is read from: its git and common folders and where its `.git` is, not the
	/// parts its config makes (the work tree, `bare_and_worktree`), which a config moved aside while
	/// it was first looked at (a Windows submodule deinit) can make differ with nothing changed.
	ReadFrom,
	/// Only the folders' identities: a layout whose config may still be moved aside, checked before
	/// it's restored.
	Identities,
}

/// [`revalidate_repository_layout`] for a layout found as `inspection` says, compared as `compared`
/// says.
async fn revalidate_layout_as(
	layout: &RepositoryLayout,
	expected: RepositoryLayoutIdentity,
	inspection: Inspection,
	compared: Compared,
) -> Result<(Dir, Dir, Option<Dir>)> {
	// Where the `.git` was found, which `core.worktree` can tell apart from the work tree; a git
	// directory found as itself is re-inspected as itself.
	let root = layout.marker_root.as_deref().unwrap_or(&layout.common_dir);
	let subject = if layout.worktree_root.is_some() {
		"worktree"
	} else {
		"repository"
	};
	let current = inspect_as(root, inspection).await.with_context(|| {
		format!(
			"{subject} changed while waiting for repository setup: {}",
			root.display()
		)
	})?;
	let same = match compared {
		Compared::Whole => &current == layout,
		Compared::ReadFrom => {
			current.git_dir == layout.git_dir
				&& current.common_dir == layout.common_dir
				&& current.marker_root == layout.marker_root
		}
		Compared::Identities => true,
	};
	if !same {
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
/// The native path remains separate from the byte-preserving Git path used for pathspec matching.
pub async fn discover_worktree_with_prefix(
	start: &Path,
) -> Result<(RepositoryLayout, PathBuf, GitPath)> {
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
	let prefix = crate::git_path::repository_path_from_native(
		start.strip_prefix(work).unwrap_or(Path::new("")),
	)?;
	Ok((found, start, prefix))
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

	/// A source whose `core.worktree` puts its work tree elsewhere is named where its `.git` is, so a
	/// clone's origin can be found again (the work tree holds no `.git`).
	#[test]
	fn local_source_url_names_where_the_git_is_not_the_work_tree() {
		let layout = RepositoryLayout {
			bare_and_worktree: false,
			worktree_root: Some(PathBuf::from("/elsewhere/other")),
			marker_root: Some(PathBuf::from("/projects/repo")),
			git_dir: PathBuf::from("/projects/repo/.git"),
			common_dir: PathBuf::from("/projects/repo/.git"),
		};
		assert_eq!(local_source_url(&layout).unwrap(), "/projects/repo");
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

	/// A command's discovery reads the config under the setup lock: while a publisher holds the lock
	/// with the config moved aside (as a Windows submodule deinit does), discovery waits, then finds
	/// the work tree `core.worktree` names, not the folder holding `.git`.
	#[tokio::test]
	async fn discovery_waits_for_a_publisher_with_the_config_moved_aside() {
		let temporary = tempfile::tempdir().unwrap();
		let repo = temporary.path().join("repo");
		let other = temporary.path().join("other");
		std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
		std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
		std::fs::create_dir_all(&other).unwrap();
		std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			repo.join(".git/config"),
			"[core]\n\trepositoryformatversion = 0\n\tworktree = ../../other\n",
		)
		.unwrap();
		let git_dir = std::fs::canonicalize(repo.join(".git")).unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();
		std::fs::rename(git_dir.join("config"), git_dir.join("config.aside")).unwrap();

		let found = discover(&repo);
		let publish = async {
			// Long enough for discovery to read the gap and reach the lock.
			tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(500)))
				.await
				.unwrap();
			std::fs::rename(git_dir.join("config.aside"), git_dir.join("config")).unwrap();
			drop(mutation);
		};
		let (layout, ()) = tokio::join!(found, publish);

		assert_eq!(
			layout.unwrap().worktree_root,
			Some(std::fs::canonicalize(&other).unwrap())
		);
	}

	/// A plain repository at `path`, its config reading `config`.
	fn plain_repository(path: &Path, config: &str) {
		std::fs::create_dir_all(path.join(".git/objects")).unwrap();
		std::fs::create_dir_all(path.join(".git/refs")).unwrap();
		std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(path.join(".git/config"), config).unwrap();
	}

	/// A repository put in place of the one found while discovery waits on its lock isn't taken for
	/// it: discovery refuses, as command setup does.
	#[tokio::test]
	async fn discovery_refuses_a_repository_put_in_place_while_it_waits() {
		let temporary = tempfile::tempdir().unwrap();
		let repo = temporary.path().join("repo");
		plain_repository(&repo, "[core]\n\trepositoryformatversion = 0\n");
		let git_dir = std::fs::canonicalize(repo.join(".git")).unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();

		let found = discover(&repo);
		let replace = async {
			tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(500)))
				.await
				.unwrap();
			std::fs::rename(&repo, temporary.path().join("repo-retired")).unwrap();
			plain_repository(&repo, "[core]\n\trepositoryformatversion = 0\n");
			drop(mutation);
		};
		let (result, ()) = tokio::join!(found, replace);

		let error = result.expect_err("a repository put in place must be refused");
		assert!(
			error
				.to_string()
				.contains("changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
	}

	/// The folder a command started in, put in its place while discovery waits on the lock, isn't taken
	/// for it: discovery refuses, as command setup does.
	#[tokio::test]
	async fn discovery_refuses_a_starting_folder_replaced_while_it_waits() {
		let temporary = tempfile::tempdir().unwrap();
		let repo = temporary.path().join("repo");
		plain_repository(&repo, "[core]\n\trepositoryformatversion = 0\n");
		let nested = repo.join("nested");
		std::fs::create_dir(&nested).unwrap();
		let git_dir = std::fs::canonicalize(repo.join(".git")).unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();

		let found = discover(&nested);
		let replace = async {
			tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(500)))
				.await
				.unwrap();
			std::fs::rename(&nested, repo.join("nested-retired")).unwrap();
			std::fs::create_dir(&nested).unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(found, replace);

		let error = result.expect_err("a starting folder put in place must be refused");
		assert!(
			error
				.to_string()
				.contains("command working directory changed while waiting"),
			"unexpected error: {error:#}"
		);
	}

	/// A local source is checked after its lock on what it's read from: a config moved aside while
	/// it was first looked at (as a Windows submodule deinit does) doesn't fail it once it's back.
	#[tokio::test]
	async fn a_source_s_config_gap_doesn_t_fail_its_setup() {
		let temporary = tempfile::tempdir().unwrap();
		let repo = temporary.path().join("repo");
		std::fs::create_dir_all(temporary.path().join("other")).unwrap();
		plain_repository(
			&repo,
			"[core]\n\trepositoryformatversion = 0\n\tworktree = ../../other\n",
		);
		let git_dir = std::fs::canonicalize(repo.join(".git")).unwrap();
		let common = Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap();
		let mutation = acquire_submodule_config_mutation_lease(&common, &git_dir).unwrap();
		std::fs::rename(git_dir.join("config"), git_dir.join("config.aside")).unwrap();
		let layout = inspect_root(&repo).await.unwrap();
		assert_eq!(
			layout.worktree_root,
			Some(std::fs::canonicalize(&repo).unwrap())
		);
		let identity = capture_repository_layout_identity(&layout).unwrap();

		let setup = revalidated_local_source_setup(&layout, identity, None, Inspection::Root);
		let publish = async {
			tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(300)))
				.await
				.unwrap();
			std::fs::rename(git_dir.join("config.aside"), git_dir.join("config")).unwrap();
			drop(mutation);
		};
		let (result, ()) = tokio::join!(setup, publish);
		result.expect("the source is as it was, its config back");
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

		let operation = revalidated_local_source_setup(&layout, identity, None, Inspection::Root);
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
