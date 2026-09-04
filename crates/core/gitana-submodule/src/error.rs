use std::path::PathBuf;

use thiserror::Error;

/// A structured failure from a submodule consumer operation.
#[derive(Debug, Error)]
pub enum SubmoduleError {
	#[error("submodule operations require a working tree")]
	BareRepository,
	#[error("opening {path}: {source}")]
	Open {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
	#[error("reading submodule metadata: {0}")]
	Worktree(#[from] gitana_worktree::WorktreeError),
	#[error("reading repository metadata: {0}")]
	Repository(#[from] gitana_repository::RepositoryError),
	#[error("reading repository objects: {0}")]
	ObjectStore(#[from] gitana_object_store::ObjectStoreError),
	#[error("invalid .gitmodules: {0}")]
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
	#[error("submodule filesystem error at {path}: {source}")]
	Io {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
}
