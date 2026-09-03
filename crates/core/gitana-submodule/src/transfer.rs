use std::path::PathBuf;

use cap_std::fs::Dir;
use gitana_config::GitConfig;
use gitana_object::HashKind;

use crate::{SubmoduleMutationLease, SubmoduleObjectId};

/// A request to validate and retain a source before publishing any staging namespace.
#[derive(Clone)]
pub struct PrepareSource {
	pub source_url: String,
	pub persist_url: String,
	pub hash_kind: HashKind,
	/// Serialized effective superproject configuration used for rewriting and authorization.
	pub config: GitConfig,
}

/// A retained transfer source together with the credential-safe endpoint identity selected for it.
///
/// The state machine records `resolved_source` in its durable intent. Frontends must derive it
/// without contacting the source and return the same value after preparation, so recovery can
/// validate an already-published repository even when its transport is unavailable.
pub struct PreparedTransfer<S> {
	pub source: S,
	pub resolved_source: String,
}

/// The effective source configuration used to fetch an existing module repository.
pub struct FetchSource {
	pub source_url: String,
	pub worktree_dir: PathBuf,
	pub config: GitConfig,
}

/// The credential-safe endpoint identity used by a completed existing-repository fetch.
pub struct FetchedTransfer {
	pub resolved_source: String,
}

/// A request to populate a repository-only clone through an already-open staging directory.
pub struct PrepareRepository {
	pub git_dir: Dir,
	pub display_git_dir: PathBuf,
	pub hash_kind: HashKind,
	pub recorded: SubmoduleObjectId,
}

/// A request to fetch a recorded commit into an existing module repository.
pub struct FetchRepository {
	pub source: FetchSource,
	pub git_dir: Dir,
	pub display_git_dir: PathBuf,
	pub hash_kind: HashKind,
	pub recorded: SubmoduleObjectId,
}

/// Transport capability injected into the submodule state machine by a native frontend.
pub trait RepositoryTransfer {
	type Error: std::error::Error + Send + Sync + 'static;
	type PreparedSource;

	/// Resolve the credential-safe endpoint identity selected by `request` without opening or
	/// contacting it.
	fn resolve_source_identity(&self, request: &PrepareSource) -> Result<String, Self::Error>;

	/// Resolve the credential-safe endpoint identity selected for an existing-module fetch without
	/// opening or contacting it.
	fn resolve_fetch_source_identity(&self, source: &FetchSource) -> Result<String, Self::Error>;

	async fn prepare_source(
		&self,
		request: PrepareSource,
		lease: SubmoduleMutationLease,
	) -> Result<PreparedTransfer<Self::PreparedSource>, Self::Error>;

	async fn populate_prepared(
		&self,
		source: Self::PreparedSource,
		request: PrepareRepository,
	) -> Result<(), Self::Error>;

	async fn fetch_recorded(
		&self,
		request: FetchRepository,
		lease: SubmoduleMutationLease,
	) -> Result<FetchedTransfer, Self::Error>;
}
