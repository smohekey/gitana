use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_config::GitConfig;
use gitana_remote::{ProtocolContext, ProtocolFromUser, ProtocolPolicy, RemoteUrl};

tokio::task_local! {
	static CURRENT_COMMAND: CommandContext;
}

/// Invocation-owned ambient state shared by a top-level command and its in-process child operations.
#[derive(Clone)]
pub struct CommandContext {
	cwd: PathBuf,
	config: Vec<String>,
	protocol_from_user: ProtocolFromUser,
	allow_protocol: Option<OsString>,
}

impl std::fmt::Debug for CommandContext {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("CommandContext")
			.field("cwd", &self.cwd)
			.field(
				"config",
				&format_args!("<{} entries redacted>", self.config.len()),
			)
			.field("protocol_from_user", &self.protocol_from_user)
			.field(
				"allow_protocol",
				&self.allow_protocol.as_ref().map(|_| "<set>"),
			)
			.finish()
	}
}

impl CommandContext {
	/// Capture the command boundary once. No deeper transport or state-machine layer reads process
	/// environment to decide protocol authorization.
	pub fn from_env(cwd: PathBuf, config: Vec<String>) -> Self {
		let protocol_from_user = match std::env::var_os("GIT_PROTOCOL_FROM_USER") {
			None => ProtocolFromUser::Allowed(true),
			Some(value) => match value.to_str() {
				Some(value) => gitana_config_native::parse_git_bool(value)
					.map(ProtocolFromUser::Allowed)
					.unwrap_or(ProtocolFromUser::InvalidValue),
				None => ProtocolFromUser::InvalidEncoding,
			},
		};
		let allow_protocol = std::env::var_os("GIT_ALLOW_PROTOCOL");
		Self {
			cwd,
			config,
			protocol_from_user,
			allow_protocol,
		}
	}

	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	pub(crate) fn current() -> Option<Self> {
		CURRENT_COMMAND.try_with(Clone::clone).ok()
	}

	/// Validate every command-scope config entry and eagerly assemble the ambient effective stack.
	/// Mutating commands call this before dispatch so a command-scope include or another expansion
	/// error cannot surface only after repository state has been created.
	pub async fn preflight(&self) -> Result<()> {
		// Git applies `-C` before parsing configuration or dispatching the command. Validate it here so
		// clone cannot mistake a missing command directory for a destination parent to create.
		match tokio::fs::metadata(&self.cwd).await {
			Ok(metadata) if metadata.is_dir() => {}
			Ok(_) => bail!("cannot change to '{}': Not a directory", self.cwd.display()),
			Err(error) => bail!("cannot change to '{}': {error}", self.cwd.display()),
		}
		gitana_config_native::validate_command_config(&self.config)?;
		self
			.scope(async { gitana_config_native::from_ambient().await.map(|_| ()) })
			.await
	}

	/// Authorize a parsed remote in either a direct-user or repository-recursive context.
	pub fn authorize(
		&self,
		config: &GitConfig,
		remote: &RemoteUrl,
		context: ProtocolContext,
	) -> Result<()> {
		let allow_protocol = self
			.allow_protocol
			.as_ref()
			.map(|value| {
				value
					.to_str()
					.ok_or_else(|| anyhow!("GIT_ALLOW_PROTOCOL is not valid UTF-8"))
			})
			.transpose()?;
		ProtocolPolicy::new(context)
			.with_from_user_state(self.protocol_from_user)
			.with_allow_protocol(allow_protocol)
			.authorize(config, remote)
	}

	/// Install command-scoped cwd/config while `future` runs.
	pub async fn scope<F: Future>(&self, future: F) -> F::Output {
		CURRENT_COMMAND
			.scope(
				self.clone(),
				gitana_config_native::with_command_config(
					self.config.clone(),
					gitana_config_native::with_command_cwd(self.cwd.clone(), future),
				),
			)
			.await
	}
}

#[cfg(test)]
mod tests {
	use super::{CommandContext, ProtocolFromUser};

	#[test]
	fn debug_redacts_command_config_values() {
		let context = CommandContext {
			cwd: "/repo".into(),
			config: vec!["http.extraHeader=Authorization: Bearer secret".to_owned()],
			protocol_from_user: ProtocolFromUser::Allowed(true),
			allow_protocol: Some("https:ssh".into()),
		};
		let debug = format!("{context:?}");
		assert!(!debug.contains("secret"));
		assert!(!debug.contains("https:ssh"));
		assert!(debug.contains("1 entries redacted"));
	}
}
