//! Submodule consumer operations over the dedicated `gitana-submodule` state machine.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use cap_std::{ambient_authority, fs::Dir};
use gitana_fs_native::{EntryIdentity, directory_identity};
use gitana_submodule::{
	ConfigViews, ConfigurationProvider, DeinitRequest, DeinitSelection, InitNotice, InitRequest,
	SubmoduleContext, SubmoduleError, SubmoduleMutationLease, SubmoduleQuery, SubmoduleStatus,
	SubmoduleStatusState, UpdateOutcomeState, UpdateReport, UpdateRequest,
};

use crate::submodule_configuration::WorktreeConfiguration;
use crate::submodule_transfer::SubmoduleTransfer;
use crate::{CommandContext, RepositoryLayoutIdentity, git_config, repo};

pub enum Action {
	Status {
		recursive: bool,
		paths: Vec<String>,
	},
	Init {
		paths: Vec<String>,
	},
	Update {
		init: bool,
		recursive: bool,
		paths: Vec<String>,
	},
	Deinit {
		force: bool,
		all: bool,
		paths: Vec<String>,
	},
}

/// Await status while retaining shared-config serialization through every module config read.
///
/// Taking the lease by value makes the lifetime explicit even though status itself does not use it.
async fn with_setup_lease<T>(
	_setup: SubmoduleMutationLease,
	operation: impl Future<Output = T>,
) -> T {
	operation.await
}

pub async fn run(cwd: &Path, command: &CommandContext, action: Action) -> Result<()> {
	let (layout, prefix) = repo::discover_worktree_with_prefix(cwd).await?;
	let worktree_root = layout
		.worktree_root
		.as_ref()
		.ok_or_else(|| anyhow!("submodule operations require a working tree"))?
		.clone();
	let identity = repo::capture_worktree_layout_identity(&layout)?;
	match &action {
		Action::Status {
			recursive: true,
			paths,
		} => {
			return Box::pin(recursive_status(
				layout.clone(),
				identity,
				&prefix,
				SubmoduleQuery::paths(paths.clone()),
			))
			.await;
		}
		Action::Update {
			init,
			recursive: true,
			paths,
		} => {
			return Box::pin(recursive_update(
				layout.clone(),
				identity,
				&prefix,
				command,
				UpdateRequest {
					query: SubmoduleQuery::paths(paths.clone()),
					initialize: *init,
					initialize_only_active: false,
					reflog_committer: None,
				},
				None,
			))
			.await;
		}
		Action::Status {
			recursive: false, ..
		}
		| Action::Init { .. }
		| Action::Update {
			recursive: false, ..
		}
		| Action::Deinit { .. } => {}
	}
	let (setup, common, git, work) = repo::command_setup_lease(&layout, identity).await?;
	let work = work.ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
	let configuration = WorktreeConfiguration::new(
		common
			.try_clone()
			.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?,
		git
			.try_clone()
			.map_err(|error| anyhow!("opening {}: {error}", layout.git_dir.display()))?,
		&layout.common_dir,
		&layout.git_dir,
	);
	let superproject = configuration.reload().await?;
	let hash_kind = configuration.hash_kind().await?;
	let context = SubmoduleContext::new(
		layout,
		common,
		git,
		work,
		ConfigViews::new(superproject.clone()),
		prefix.clone(),
		hash_kind,
	)?;

	match action {
		Action::Status {
			recursive: false,
			paths,
		} => {
			with_setup_lease(setup, async {
				for status in context
					.status(&SubmoduleQuery::paths(paths), &configuration)
					.await?
				{
					println!(
						"{}{} {}",
						status.state.sigil(),
						status.oid,
						render_relative(&prefix, &status.path)
					);
				}
				Ok::<(), anyhow::Error>(())
			})
			.await?;
		}
		Action::Init { paths } => {
			drop(setup);
			let report = context
				.init(
					&InitRequest {
						query: SubmoduleQuery::paths(paths),
					},
					&configuration,
				)
				.await?;
			render_init_notices(&report);
			for outcome in report.outcomes {
				if let Some(url) = outcome.registered_url {
					eprintln!(
						"Submodule '{}' ({url}) registered for path '{}'",
						outcome.name,
						render_relative(&prefix, &outcome.path)
					);
				}
			}
		}
		Action::Update {
			init,
			recursive: false,
			paths,
		} => {
			drop(setup);
			// A new module starts with the ambient system/global/command stack. The superproject's
			// repository-local layers drive source rewriting and authorization, but they are not the
			// module repository's own effective configuration.
			let module_base = git_config::from_ambient().await?;
			let transfer = SubmoduleTransfer::new(command, &worktree_root, module_base, None);
			let request = UpdateRequest {
				query: SubmoduleQuery::paths(paths),
				initialize: init,
				initialize_only_active: false,
				reflog_committer: Some(committer(&superproject)),
			};
			match Box::pin(context.update(&request, &configuration, &transfer)).await {
				Ok(report) => render_update(&prefix, &report),
				Err(failure) => {
					render_update(&prefix, &failure.completed);
					return Err(failure.into());
				}
			}
		}
		Action::Deinit { force, all, paths } => {
			drop(setup);
			let request = DeinitRequest {
				selection: if all {
					DeinitSelection::All
				} else {
					DeinitSelection::Paths(paths)
				},
				force,
			};
			match context.deinit(&request, &configuration).await {
				Ok(report) => render_deinit(&prefix, &report),
				Err(failure) => {
					render_deinit(&prefix, &failure.completed);
					return Err(failure.into());
				}
			}
		}
		Action::Status {
			recursive: true, ..
		}
		| Action::Update {
			recursive: true, ..
		} => unreachable!("recursive actions return before opening the root context"),
	}
	Ok(())
}

async fn recursive_status(
	root_layout: repo::RepositoryLayout,
	root_identity: RepositoryLayoutIdentity,
	prefix: &str,
	query: SubmoduleQuery,
) -> Result<()> {
	let root = root_layout
		.worktree_root
		.as_ref()
		.expect("recursive status requires a worktree")
		.clone();
	recursive_status_level(
		root,
		None,
		Some((root_layout, root_identity)),
		prefix,
		prefix.to_owned(),
		String::new(),
		query,
	)
	.await
}

fn recursive_status_level<'a>(
	level_root: PathBuf,
	expected_git_dir: Option<PathBuf>,
	discovered_root: Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
	prefix: &'a str,
	query_prefix: String,
	level_prefix: String,
	query: SubmoduleQuery,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
	Box::pin(async move {
		let (layout, statuses) = status_level(
			&level_root,
			expected_git_dir.as_deref(),
			discovered_root,
			&query_prefix,
			&query,
			false,
			false,
		)
		.await?;
		for status in statuses {
			println!(
				"{}{} {}",
				status.state.sigil(),
				status.oid,
				render_nested_relative(prefix, &level_prefix, &status.path)
			);
			if matches!(
				status.state,
				SubmoduleStatusState::Current | SubmoduleStatusState::Modified
			) {
				recursive_status_level(
					level_root.join(&status.path),
					Some(layout.git_dir.join("modules").join(&status.name)),
					None,
					prefix,
					String::new(),
					join_submodule_path(&level_prefix, &status.path),
					SubmoduleQuery::all(),
				)
				.await?;
			}
		}
		Ok(())
	})
}

async fn recursive_update(
	root_layout: repo::RepositoryLayout,
	root_identity: RepositoryLayoutIdentity,
	prefix: &str,
	command: &CommandContext,
	request: UpdateRequest,
	credential_url_base: Option<String>,
) -> Result<()> {
	let UpdateRequest {
		query,
		initialize,
		initialize_only_active,
		..
	} = request;
	let root = root_layout
		.worktree_root
		.as_ref()
		.expect("recursive update requires a worktree")
		.clone();
	let root_proof = (root_layout, root_identity);
	let module_base = Box::pin(git_config::from_ambient()).await?;
	let recovery_levels = Box::pin(initialized_subtree(
		&root,
		prefix,
		query.clone(),
		root_proof.clone(),
	))
	.await?;
	for (level_root, expected_git_dir, discovered_root, level_prefix) in recovery_levels {
		Box::pin(resume_update_level(
			&level_root,
			expected_git_dir.as_deref(),
			discovered_root,
			prefix,
			&level_prefix,
			command,
			module_base.clone(),
		))
		.await?;
	}

	let mut pending = VecDeque::from([(
		root,
		None,
		Some(root_proof),
		prefix.to_owned(),
		String::new(),
		query,
		initialize_only_active,
		credential_url_base,
	)]);
	while let Some((
		level_root,
		expected_git_dir,
		discovered_root,
		query_prefix,
		level_prefix,
		query,
		initialize_only_active,
		credential_url_base,
	)) = pending.pop_front()
	{
		let (layout, report, mut descendant_url_bases) = Box::pin(update_level(
			&level_root,
			expected_git_dir.as_deref(),
			discovered_root,
			(&query_prefix, prefix, &level_prefix),
			command,
			(module_base.clone(), credential_url_base),
			UpdateRequest {
				query,
				initialize,
				initialize_only_active,
				reflog_committer: None,
			},
		))
		.await?;
		for outcome in report.outcomes {
			if matches!(
				outcome.state,
				UpdateOutcomeState::Cloned
					| UpdateOutcomeState::CheckedOut
					| UpdateOutcomeState::AlreadyCurrent
			) {
				pending.push_back((
					level_root.join(&outcome.path),
					Some(layout.git_dir.join("modules").join(&outcome.name)),
					None,
					String::new(),
					join_submodule_path(&level_prefix, &outcome.path),
					SubmoduleQuery::all(),
					false,
					descendant_url_bases.remove(&outcome.path),
				));
			}
		}
	}
	Ok(())
}

/// Persist Git's all-submodules activation and then initialize every submodule in a newly published
/// clone while retaining the exact repository identity established by clone publication.
pub(crate) async fn update_published_clone(
	root_layout: repo::RepositoryLayout,
	root_identity: RepositoryLayoutIdentity,
	command: &CommandContext,
	credential_url_base: Option<String>,
) -> Result<()> {
	let (lease, common, git, _) = Box::pin(repo::command_config_mutation_lease(
		&root_layout,
		root_identity,
	))
	.await?;
	repo::ensure_no_pending_deinit_at(&root_layout, &common, &git)?;
	let configuration =
		WorktreeConfiguration::new(common, git, &root_layout.common_dir, &root_layout.git_dir);
	configuration.apply_init(&[], true, lease).await?;
	repo::revalidate_repository_layout(&root_layout, root_identity).await?;

	recursive_update(
		root_layout,
		root_identity,
		"",
		command,
		UpdateRequest {
			query: SubmoduleQuery::all(),
			initialize: true,
			initialize_only_active: true,
			reflog_committer: None,
		},
		credential_url_base,
	)
	.await
}

async fn initialized_subtree(
	root: &Path,
	root_prefix: &str,
	query: SubmoduleQuery,
	root_proof: (repo::RepositoryLayout, RepositoryLayoutIdentity),
) -> Result<
	Vec<(
		PathBuf,
		Option<PathBuf>,
		Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
		String,
	)>,
> {
	let mut levels = Vec::new();
	let mut pending = vec![(
		root.to_owned(),
		None,
		Some(root_proof),
		root_prefix.to_owned(),
		String::new(),
		query,
		0_usize,
	)];
	while let Some((
		level_root,
		expected_git_dir,
		discovered_root,
		query_prefix,
		level_prefix,
		query,
		depth,
	)) = pending.pop()
	{
		let returned_root = discovered_root.clone();
		let (layout, statuses) = Box::pin(status_level(
			&level_root,
			expected_git_dir.as_deref(),
			discovered_root,
			&query_prefix,
			&query,
			true,
			depth == 0,
		))
		.await?;
		levels.push((
			level_root.clone(),
			expected_git_dir,
			returned_root,
			level_prefix.clone(),
			depth,
		));
		let mut children = Vec::new();
		for status in statuses {
			if matches!(
				status.state,
				SubmoduleStatusState::Current | SubmoduleStatusState::Modified
			) {
				children.push((
					level_root.join(&status.path),
					Some(layout.git_dir.join("modules").join(&status.name)),
					None,
					String::new(),
					join_submodule_path(&level_prefix, &status.path),
					SubmoduleQuery::all(),
					depth + 1,
				));
			}
		}
		pending.extend(children.into_iter().rev());
	}
	levels.sort_by_key(|(_, _, _, _, depth)| std::cmp::Reverse(*depth));
	Ok(
		levels
			.into_iter()
			.map(|(root, expected, discovered, prefix, _)| (root, expected, discovered, prefix))
			.collect(),
	)
}

async fn status_level(
	root: &Path,
	expected_git_dir: Option<&Path>,
	discovered_root: Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
	query_prefix: &str,
	query: &SubmoduleQuery,
	reject_deinit: bool,
	include_pending_owner: bool,
) -> Result<(repo::RepositoryLayout, Vec<SubmoduleStatus>)> {
	let (layout, setup, common, git, work, configuration, superproject, hash_kind) =
		Box::pin(open_level(root, expected_git_dir, discovered_root)).await?;
	if reject_deinit {
		repo::ensure_no_pending_deinit_at(&layout, &common, &git)?;
	}
	let returned_layout = layout.clone();
	let context = SubmoduleContext::new(
		layout,
		common,
		git,
		work,
		ConfigViews::new(superproject),
		query_prefix.to_owned(),
		hash_kind,
	)?;
	let pending_owner = if include_pending_owner {
		context.pending_update_query()?
	} else {
		None
	};
	let statuses = with_setup_lease(setup, async move {
		let mut statuses = context.status(query, &configuration).await?;
		if let Some(owner) = pending_owner {
			for status in context.status(&owner, &configuration).await? {
				if !statuses.iter().any(|current| current.path == status.path) {
					statuses.push(status);
				}
			}
		}
		Ok::<_, SubmoduleError>(statuses)
	})
	.await
	.map_err(anyhow::Error::from)?;
	Ok((returned_layout, statuses))
}

async fn resume_update_level(
	root: &Path,
	expected_git_dir: Option<&Path>,
	discovered_root: Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
	prefix: &str,
	level_prefix: &str,
	command: &CommandContext,
	module_base: gitana_config::GitConfig,
) -> Result<()> {
	let (layout, setup, common, git, work, configuration, superproject, hash_kind) =
		Box::pin(open_level(root, expected_git_dir, discovered_root)).await?;
	repo::ensure_no_pending_deinit_at(&layout, &common, &git)?;
	drop(setup);
	let context = SubmoduleContext::new(
		layout,
		common,
		git,
		work,
		ConfigViews::new(superproject.clone()),
		String::new(),
		hash_kind,
	)?;
	let transfer = SubmoduleTransfer::new(command, root, module_base, None);
	match Box::pin(context.resume_pending_update(
		&configuration,
		&transfer,
		Some(committer(&superproject)),
	))
	.await
	{
		Ok(Some(report)) => render_update_at(prefix, level_prefix, &report),
		Ok(None) => {}
		Err(failure) => {
			render_update_at(prefix, level_prefix, &failure.completed);
			return Err(failure.into());
		}
	}
	Ok(())
}

async fn update_level(
	root: &Path,
	expected_git_dir: Option<&Path>,
	discovered_root: Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
	scope: (&str, &str, &str),
	command: &CommandContext,
	transfer_state: (gitana_config::GitConfig, Option<String>),
	mut request: UpdateRequest,
) -> Result<(
	repo::RepositoryLayout,
	UpdateReport,
	HashMap<String, String>,
)> {
	let (query_prefix, prefix, level_prefix) = scope;
	let (module_base, credential_url_base) = transfer_state;
	let (layout, setup, common, git, work, configuration, superproject, hash_kind) =
		Box::pin(open_level(root, expected_git_dir, discovered_root)).await?;
	repo::ensure_no_pending_deinit_at(&layout, &common, &git)?;
	drop(setup);
	let returned_layout = layout.clone();
	let context = SubmoduleContext::new(
		layout,
		common,
		git,
		work,
		ConfigViews::new(superproject.clone()),
		query_prefix.to_owned(),
		hash_kind,
	)?;
	request.reflog_committer = Some(committer(&superproject));
	let transfer = SubmoduleTransfer::new(command, root, module_base, credential_url_base);
	match Box::pin(context.update(&request, &configuration, &transfer)).await {
		Ok(report) => {
			let descendant_url_bases = transfer.take_descendant_url_bases()?;
			render_update_at(prefix, level_prefix, &report);
			Ok((returned_layout, report, descendant_url_bases))
		}
		Err(failure) => {
			render_update_at(prefix, level_prefix, &failure.completed);
			Err(failure.into())
		}
	}
}

async fn open_level(
	root: &Path,
	expected_git_dir: Option<&Path>,
	discovered_root: Option<(repo::RepositoryLayout, RepositoryLayoutIdentity)>,
) -> Result<(
	repo::RepositoryLayout,
	SubmoduleMutationLease,
	Dir,
	Dir,
	Dir,
	WorktreeConfiguration,
	gitana_config::GitConfig,
	gitana_object::HashKind,
)> {
	let (layout, identity) = match discovered_root {
		Some((layout, identity)) => {
			validate_recursive_level_layout(root, expected_git_dir, &layout)?;
			(layout, identity)
		}
		None => {
			let layout = Box::pin(repo::inspect_root(root)).await?;
			validate_recursive_level_layout(root, expected_git_dir, &layout)?;
			let identity = repo::capture_worktree_layout_identity(&layout)?;
			(layout, identity)
		}
	};
	let (setup, common, git, work) = Box::pin(repo::command_setup_lease(&layout, identity)).await?;
	let work = work.ok_or_else(|| anyhow!("submodule operations require a working tree"))?;
	let configuration = WorktreeConfiguration::new(
		common
			.try_clone()
			.map_err(|error| anyhow!("opening {}: {error}", layout.common_dir.display()))?,
		git
			.try_clone()
			.map_err(|error| anyhow!("opening {}: {error}", layout.git_dir.display()))?,
		&layout.common_dir,
		&layout.git_dir,
	);
	let superproject = Box::pin(configuration.reload()).await?;
	let hash_kind = Box::pin(configuration.hash_kind()).await?;
	setup.validate()?;
	Ok((
		layout,
		setup,
		common,
		git,
		work,
		configuration,
		superproject,
		hash_kind,
	))
}

fn validate_recursive_level_layout(
	root: &Path,
	expected_git_dir: Option<&Path>,
	layout: &repo::RepositoryLayout,
) -> Result<()> {
	let Some(worktree_root) = layout.worktree_root.as_deref() else {
		return Err(anyhow!(
			"submodule worktree changed while entering recursive operation: {}",
			root.display()
		));
	};
	if directory_identity_at(root, "submodule worktree")?
		!= directory_identity_at(worktree_root, "discovered submodule worktree")?
	{
		return Err(anyhow!(
			"submodule worktree changed while entering recursive operation: {}",
			root.display()
		));
	}
	if let Some(expected_git_dir) = expected_git_dir {
		let expected = directory_identity_at(expected_git_dir, "expected module repository")?;
		if directory_identity_at(&layout.git_dir, "discovered module Git directory")? != expected
			|| directory_identity_at(&layout.common_dir, "discovered module common directory")?
				!= expected
		{
			return Err(anyhow!(
				"submodule repository attachment changed while entering recursive operation: {}",
				root.display()
			));
		}
	}
	Ok(())
}

fn directory_identity_at(path: &Path, kind: &str) -> Result<EntryIdentity> {
	let directory = Dir::open_ambient_dir(path, ambient_authority())
		.map_err(|error| anyhow!("opening {kind} {}: {error}", path.display()))?;
	directory_identity(&directory)
		.map_err(|error| anyhow!("identifying {kind} {}: {error}", path.display()))
}

fn render_deinit(prefix: &str, report: &gitana_submodule::DeinitReport) {
	for outcome in &report.outcomes {
		let path = render_relative(prefix, &outcome.path);
		if outcome.cleared {
			println!("Cleared directory '{path}'");
		}
		if outcome.unregistered {
			eprintln!(
				"Submodule '{}' unregistered for path '{path}'",
				outcome.name
			);
		}
	}
}

fn render_update(prefix: &str, report: &gitana_submodule::UpdateReport) {
	render_update_at(prefix, "", report);
}

fn render_update_at(prefix: &str, level_prefix: &str, report: &gitana_submodule::UpdateReport) {
	render_init_notices(&report.initialization);
	for outcome in &report.initialization.outcomes {
		if let Some(url) = &outcome.registered_url {
			eprintln!(
				"Submodule '{}' ({url}) registered for path '{}'",
				outcome.name,
				render_nested_relative(prefix, level_prefix, &outcome.path)
			);
		}
	}
	for outcome in &report.outcomes {
		match outcome.state {
			UpdateOutcomeState::Cloned | UpdateOutcomeState::CheckedOut => println!(
				"Submodule path '{}': checked out '{}'",
				render_nested_relative(prefix, level_prefix, &outcome.path),
				outcome.recorded
			),
			UpdateOutcomeState::SkippedByStrategy => eprintln!(
				"Skipping submodule '{}'",
				render_nested_relative(prefix, level_prefix, &outcome.path)
			),
			UpdateOutcomeState::SkippedUnregistered
			| UpdateOutcomeState::SkippedInactive
			| UpdateOutcomeState::AlreadyCurrent => {}
		}
	}
}

fn render_nested_relative(prefix: &str, level_prefix: &str, path: &str) -> String {
	render_relative(prefix, &join_submodule_path(level_prefix, path))
}

fn join_submodule_path(prefix: &str, path: &str) -> String {
	match (prefix.is_empty(), path.is_empty()) {
		(true, _) => path.to_owned(),
		(_, true) => prefix.to_owned(),
		(false, false) => format!("{prefix}/{path}"),
	}
}

fn render_init_notices(report: &gitana_submodule::InitReport) {
	for notice in &report.notices {
		match notice {
			InitNotice::AuthoritativeSuperproject { missing_key } => eprintln!(
				"warning: could not look up configuration '{missing_key}'. Assuming this repository is its own authoritative upstream."
			),
		}
	}
}

fn render_relative(prefix: &str, path: &str) -> String {
	let prefix: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
	let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
	let common = prefix.iter().zip(&path).take_while(|(a, b)| a == b).count();
	let mut rendered = "../".repeat(prefix.len() - common);
	rendered.push_str(&path[common..].join("/"));
	if rendered.is_empty() {
		"./".to_owned()
	} else {
		rendered
	}
}

fn committer(config: &gitana_config::GitConfig) -> String {
	let seconds = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_secs())
		.unwrap_or(0);
	gitana_identity::signature_or_default(
		std::env::var("GIT_COMMITTER_NAME").ok(),
		std::env::var("GIT_COMMITTER_EMAIL").ok(),
		Some(config),
		&std::env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| format!("{seconds} +0000")),
	)
}

#[cfg(test)]
mod tests {
	use std::process::Command;
	use std::sync::Arc;

	use gitana_submodule::SubmoduleMutationLease;

	use super::{open_level, render_relative, validate_recursive_level_layout, with_setup_lease};
	use crate::repo;

	#[tokio::test]
	async fn setup_lease_is_retained_until_status_completes() {
		let owner = Arc::new(());
		let retained = Arc::downgrade(&owner);
		let lease = SubmoduleMutationLease::retain(Arc::clone(&owner));
		drop(owner);

		with_setup_lease(lease, async {
			tokio::task::yield_now().await;
			assert!(retained.upgrade().is_some());
		})
		.await;

		assert!(retained.upgrade().is_none());
	}

	#[tokio::test]
	async fn recursive_root_open_rejects_a_replacement_after_discovery() {
		let temporary = tempfile::tempdir().unwrap();
		let root = temporary.path().join("root");
		std::fs::create_dir(&root).unwrap();
		let initialized = Command::new("git")
			.args(["-C", root.to_str().unwrap(), "init", "-q"])
			.output()
			.unwrap();
		assert!(initialized.status.success());
		let layout = repo::inspect_root(&root).await.unwrap();
		let root = layout
			.worktree_root
			.as_ref()
			.expect("initialized worktree")
			.clone();
		let identity = repo::capture_worktree_layout_identity(&layout).unwrap();

		std::fs::rename(&root, temporary.path().join("displaced")).unwrap();
		std::fs::create_dir(&root).unwrap();
		let replacement = Command::new("git")
			.args(["-C", root.to_str().unwrap(), "init", "-q"])
			.output()
			.unwrap();
		assert!(replacement.status.success());

		let error = match open_level(&root, None, Some((layout, identity))).await {
			Ok(_) => panic!("the discovered root identity must remain binding"),
			Err(error) => error,
		};
		assert!(
			format!("{error:#}").contains("worktree changed while waiting for repository setup"),
			"unexpected error: {error:#}"
		);
	}

	#[tokio::test]
	async fn recursive_child_open_rejects_a_redirected_common_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let root = temporary.path().join("child");
		let module = temporary.path().join("module.git");
		let foreign = temporary.path().join("foreign.git");
		std::fs::create_dir(&root).unwrap();
		for repository in [&module, &foreign] {
			let initialized = Command::new("git")
				.args(["init", "--bare", "-q"])
				.arg(repository)
				.output()
				.unwrap();
			assert!(initialized.status.success());
		}
		std::fs::write(root.join(".git"), format!("gitdir: {}\n", module.display())).unwrap();
		std::fs::write(module.join("commondir"), format!("{}\n", foreign.display())).unwrap();
		let root = std::fs::canonicalize(root).unwrap();
		let module = std::fs::canonicalize(module).unwrap();
		let foreign_config = std::fs::read(foreign.join("config")).unwrap();

		let error = match open_level(&root, Some(&module), None).await {
			Ok(_) => panic!("a recursive child must not redirect its common directory"),
			Err(error) => error,
		};
		assert!(
			format!("{error:#}").contains("submodule repository attachment changed"),
			"unexpected error: {error:#}"
		);
		assert_eq!(
			std::fs::read(foreign.join("config")).unwrap(),
			foreign_config
		);
	}

	#[test]
	fn recursive_layout_accepts_equivalent_directory_spellings() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("worktree");
		let module = temporary.path().join("module.git");
		std::fs::create_dir(&worktree).unwrap();
		std::fs::create_dir(&module).unwrap();
		let layout = repo::RepositoryLayout {
			worktree_root: Some(std::fs::canonicalize(&worktree).unwrap()),
			git_dir: std::fs::canonicalize(&module).unwrap(),
			common_dir: std::fs::canonicalize(&module).unwrap(),
		};

		validate_recursive_level_layout(&worktree.join("."), Some(&module.join(".")), &layout).unwrap();
	}

	#[test]
	fn paths_are_rendered_from_the_invocation_prefix() {
		assert_eq!(render_relative("libs", "libs/one"), "one");
		assert_eq!(render_relative("libs/nested", "top"), "../../top");
		assert_eq!(render_relative("libs", "libs"), "./");
	}
}
