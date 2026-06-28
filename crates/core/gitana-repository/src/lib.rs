//! The git repository engine: git semantics over the storage layer.
//!
//! Composes a repo-scoped [`gitana_object_store::GitObjectStore`] (object graph)
//! with refs/HEAD over the file store into a [`Repository`], in a git-compatible
//! sha256 layout (see docs/hlds/repository-engine.md). This phase covers
//! init/open + config + loose refs with CAS; object-graph construction, the
//! reflog, revision resolution, history walk, and packed-refs reading land in
//! later phases.

mod config;
mod error;
mod head;
mod mode;
mod refs;
mod repository;
mod revision;
mod tree;

pub use self::{
	config::Config, error::RepositoryError, head::HeadState, mode::FileMode, refs::RefStore,
	repository::Repository, tree::TreeBuildEntry,
};
