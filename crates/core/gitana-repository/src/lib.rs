//! The git repository engine: git semantics over the storage layer.
//!
//! Composes a repo-scoped [`gitana_object_store::ObjectStore`] (object graph)
//! with refs/HEAD over the file store into a [`Repository`], in a git-compatible
//! sha256 layout (see docs/hlds/repository-engine.md). This phase covers
//! init/open + config + loose refs with CAS; object-graph construction, the
//! reflog, revision resolution, history walk, and packed-refs reading land in
//! later phases.

mod config;
mod detect;
mod error;
mod head;
mod merge;
mod merge_base;
mod merge_state;
mod mode;
mod object_source;
mod rebase_state;
mod refs;
mod repository;
mod revision;
mod tree;

pub use self::{
	config::Config, detect::detect_hash_kind, error::RepositoryError, head::HeadState,
	merge::TreeMerge, mode::FileMode, rebase_state::RebaseState, refs::RefStore,
	repository::Repository, tree::TreeBuildEntry,
};
