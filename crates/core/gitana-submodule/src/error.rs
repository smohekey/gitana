use std::path::PathBuf;

use thiserror::Error;

/// A structured failure from a submodule consumer operation.
///
/// A variant wrapping another error says only its own context; the wrapped error is its
/// [`source`](std::error::Error::source), so a chain printer (gta's, anyhow's `{:#}`) shows each
/// message once. A plain `{}` shows the context alone.
#[derive(Debug, Error)]
pub enum SubmoduleError {
	#[error("submodule operations require a working tree")]
	BareRepository,
	#[error("opening {path}")]
	Open {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
	#[error("reading submodule metadata")]
	Worktree(#[from] gitana_worktree::WorktreeError),
	#[error("reading repository metadata")]
	Repository(#[from] gitana_repository::RepositoryError),
	#[error("reading repository objects")]
	ObjectStore(#[from] gitana_object_store::ObjectStoreError),
	#[error("invalid .gitmodules")]
	Config(#[from] gitana_config::ConfigError),
	#[error("accessing superproject configuration: {0}")]
	Configuration(String),
	#[error("missing value for '{0}'")]
	MissingValue(String),
	#[error("no submodule mapping found in .gitmodules for path '{0}'")]
	MissingMapping(String),
	#[error("multiple .gitmodules entries map to submodule path '{0}'")]
	DuplicateMapping(String),
	#[error("ambiguous .gitmodules declarations collide at '{0}'")]
	AmbiguousDeclaration(String),
	#[error("no URL found in .gitmodules for submodule path '{0}'")]
	MissingUrl(String),
	#[error("no superproject remote is available to resolve relative URL '{url}' for '{path}'")]
	MissingRelativeBase { path: String, url: String },
	#[error("relative submodule URL '{url}' for '{path}' escapes its superproject remote base")]
	InvalidRelativeUrl { path: String, url: String },
	#[error("unsupported submodule update strategy '{strategy}' for '{name}'")]
	UnsupportedStrategy { name: String, strategy: String },
	#[error("submodule gitlink '{0}' is conflicted")]
	Conflicted(String),
	#[error("submodule '{0}' is not registered")]
	Unregistered(String),
	#[error("submodule path '{0}' contains foreign or non-empty content")]
	ForeignMount(String),
	#[error("submodule repository for '{0}' is corrupt or has an incompatible object format")]
	InvalidRepository(String),
	#[error("cannot update existing submodule '{0}' because its HEAD is unborn")]
	UnbornModuleHead(String),
	#[error("existing submodule '{0}' has no remote.origin.url")]
	MissingModuleOrigin(String),
	#[error("submodule '{name}' has no URL for remote '{remote}'")]
	MissingModuleRemote { name: String, remote: String },
	#[error("submodule '{name}' has no locally available target for '{target}'")]
	MissingUpdateTarget { name: String, target: String },
	#[error("submodule '{0}' uses branch='.' but the superproject HEAD is detached")]
	DetachedSuperproject(String),
	#[error("submodule pointer path '{}' is not representable as UTF-8", .0.display())]
	UnrepresentablePointerPath(PathBuf),
	#[error("submodule update is already running for this worktree")]
	UpdateLocked,
	#[error("submodule update depth must be a positive number of commits")]
	InvalidDepth,
	#[error(
		"submodule path '{0}' contains local modifications; use --force to deinitialize while retaining the checkout and its local changes"
	)]
	LocalModifications(String),
	#[error("cannot update submodule '{name}' while {operation} is in progress")]
	OperationInProgress {
		name: String,
		operation: &'static str,
	},
	#[error("{0}")]
	Merge(String),
	#[error("{0}")]
	MergeConflict(Box<crate::UpdateMergeConflict>),
	#[error("submodule update recovery state does not match the requested operation: {0}")]
	RecoveryRequired(String),
	#[error("repository transfer failed: {0}")]
	Transfer(String),
	#[error("pathspec '{0}' did not match any file known to git")]
	PathspecNoMatch(String),
	#[error("submodule deinit requires an explicit all selection or at least one pathspec")]
	EmptyDeinitSelection,
	#[error("unsafe submodule name '{0}'")]
	UnsafeName(String),
	#[error("unsafe submodule path '{0}'")]
	UnsafePath(String),
	/// A nested submodule's `.git` directory has no `.gitmodules` declaration to name its place in
	/// its parent's `modules/`, so deinit can't absorb it (git's message).
	#[error("could not lookup name for submodule '{0}'")]
	AbsorbUnnamed(String),
	/// A nested submodule's `.git` directory can't be absorbed into `to`, which isn't empty (git's
	/// message).
	#[error(
		"could not migrate git directory from '{}' to '{}': Directory not empty",
		.from.display(),
		.to.display()
	)]
	AbsorbBlocked { from: PathBuf, to: PathBuf },
	#[error("submodule filesystem error at {path}")]
	Io {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
}

/// `error`'s message followed by each of its sources', joined as a chain printer joins them: for an
/// error frozen into a string, which keeps no source of its own to show the rest.
pub(crate) fn with_causes(error: &(dyn std::error::Error + 'static)) -> String {
	let mut text = error.to_string();
	let mut source = error.source();
	while let Some(cause) = source {
		text.push_str(": ");
		text.push_str(&cause.to_string());
		source = cause.source();
	}
	text
}

#[cfg(test)]
mod tests {
	use super::SubmoduleError;
	use crate::{DeinitFailure, SyncFailure, UpdateFailure};

	/// `error`'s message and each of its sources', as a chain printer shows them.
	fn chain(error: &(dyn std::error::Error + 'static)) -> Vec<String> {
		let mut messages = vec![error.to_string()];
		let mut source = error.source();
		while let Some(cause) = source {
			messages.push(cause.to_string());
			source = cause.source();
		}
		messages
	}

	fn io() -> SubmoduleError {
		SubmoduleError::Io {
			path: "/p".into(),
			source: std::io::Error::other("boom"),
		}
	}

	#[test]
	fn a_wrapping_variant_says_only_its_context_and_passes_on_the_rest() {
		assert_eq!(chain(&io()), ["submodule filesystem error at /p", "boom"]);
		let open = SubmoduleError::Open {
			path: "/p".into(),
			source: std::io::Error::other("boom"),
		};
		assert_eq!(chain(&open), ["opening /p", "boom"]);
		let parse = gitana_config::GitConfig::parse("[").unwrap_err();
		let config = SubmoduleError::from(gitana_config::GitConfig::parse("[").unwrap_err());
		assert_eq!(
			chain(&config),
			["invalid .gitmodules".to_owned(), parse.to_string()]
		);
	}

	#[test]
	fn each_failure_says_each_message_once() {
		let inner = ["submodule filesystem error at /p", "boom"];
		let named = |context: &str| {
			std::iter::once(context.to_owned())
				.chain(inner.map(str::to_owned))
				.collect::<Vec<_>>()
		};
		let deinit = DeinitFailure {
			completed: Default::default(),
			module: Some("m".into()),
			source: io(),
		};
		assert_eq!(chain(&deinit), named("deinitializing submodule 'm'"));
		let update = UpdateFailure {
			completed: Default::default(),
			module: Some("m".into()),
			source: io(),
		};
		assert_eq!(chain(&update), named("updating submodule 'm'"));
		let sync = SyncFailure {
			completed: Default::default(),
			module: Some("m".into()),
			source: io(),
		};
		assert_eq!(chain(&sync), named("synchronizing submodule 'm'"));

		// Without a module, a failure is the inner error itself.
		let preflight = DeinitFailure {
			completed: Default::default(),
			module: None,
			source: io(),
		};
		assert_eq!(chain(&preflight), inner);
		let update = UpdateFailure {
			completed: Default::default(),
			module: None,
			source: io(),
		};
		assert_eq!(chain(&update), inner);
		let sync = SyncFailure {
			completed: Default::default(),
			module: None,
			source: io(),
		};
		assert_eq!(chain(&sync), inner);
	}
}
