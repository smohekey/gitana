//! One-level submodule consumer operations over the dedicated `gitana-submodule` state machine.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use cap_std::{ambient_authority, fs::Dir};
use gitana_submodule::{
	ConfigViews, ConfigurationProvider, InitNotice, InitRequest, SubmoduleContext, SubmoduleQuery,
	UpdateOutcomeState, UpdateRequest,
};

use crate::submodule_configuration::WorktreeConfiguration;
use crate::submodule_transfer::SubmoduleTransfer;
use crate::{CommandContext, git_config, repo};

pub enum Action {
	Status { paths: Vec<String> },
	Init { paths: Vec<String> },
	Update { init: bool, paths: Vec<String> },
}

pub async fn run(cwd: &Path, command: &CommandContext, action: Action) -> Result<()> {
	let (layout, prefix) = repo::discover_worktree_with_prefix(cwd).await?;
	let worktree_root = layout
		.worktree_root
		.as_ref()
		.ok_or_else(|| anyhow!("submodule operations require a working tree"))?
		.clone();
	let common = open_dir(&layout.common_dir)?;
	let git = open_dir(&layout.git_dir)?;
	let work = open_dir(&worktree_root)?;
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
		Action::Status { paths } => {
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
		}
		Action::Init { paths } => {
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
		Action::Update { init, paths } => {
			// A new module starts with the ambient system/global/command stack. The superproject's
			// repository-local layers drive source rewriting and authorization, but they are not the
			// module repository's own effective configuration.
			let module_base = git_config::from_ambient().await?;
			let transfer =
				SubmoduleTransfer::new(command, &worktree_root, superproject.clone(), module_base);
			let request = UpdateRequest {
				query: SubmoduleQuery::paths(paths),
				initialize: init,
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
	}
	Ok(())
}

fn open_dir(path: &Path) -> Result<Dir> {
	Dir::open_ambient_dir(path, ambient_authority())
		.map_err(|error| anyhow!("opening {}: {error}", path.display()))
}

fn render_update(prefix: &str, report: &gitana_submodule::UpdateReport) {
	render_init_notices(&report.initialization);
	for outcome in &report.initialization.outcomes {
		if let Some(url) = &outcome.registered_url {
			eprintln!(
				"Submodule '{}' ({url}) registered for path '{}'",
				outcome.name,
				render_relative(prefix, &outcome.path)
			);
		}
	}
	for outcome in &report.outcomes {
		match outcome.state {
			UpdateOutcomeState::Cloned | UpdateOutcomeState::CheckedOut => println!(
				"Submodule path '{}': checked out '{}'",
				render_relative(prefix, &outcome.path),
				outcome.recorded
			),
			UpdateOutcomeState::SkippedByStrategy => eprintln!(
				"Skipping submodule '{}'",
				render_relative(prefix, &outcome.path)
			),
			UpdateOutcomeState::SkippedUnregistered
			| UpdateOutcomeState::SkippedInactive
			| UpdateOutcomeState::AlreadyCurrent => {}
		}
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
	use super::render_relative;

	#[test]
	fn paths_are_rendered_from_the_invocation_prefix() {
		assert_eq!(render_relative("libs", "libs/one"), "one");
		assert_eq!(render_relative("libs/nested", "top"), "../../top");
		assert_eq!(render_relative("libs", "libs"), "./");
	}
}
